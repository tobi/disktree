//! Bounded-memory `O(depth)` streaming filesystem walker.
//!
//! Used by `StorageMode::Streaming` (e.g. `disktree-cli --stream --ndjson`,
//! constrained containers, and the `disktree-scan` SSH helper). Never retains
//! a full tree in memory and never touches the cache DB.

use std::fs::{self, Metadata, ReadDir};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

use crate::mounts::{never_scanned, strict_mount_boundaries_for};
use crate::types::{NodeKind, ScanOptions};

/// Fine-grained options for [`stream_walk_with_config`].
#[derive(Clone, Debug)]
pub struct StreamConfig {
    /// Additional paths excluded before `stat`/`read_dir`.
    pub excluded: FxHashSet<PathBuf>,
    /// Deduplicate hardlinks inside the walker. When `false`, hardlink
    /// identities are emitted on each [`StreamItem::Entry`] without keeping
    /// a per-file link table in memory (used by SSH `client_dedup`).
    pub dedup_in_walker: bool,
    /// Optional cap on the in-walker hardlink table before failing explicitly.
    pub max_pending_links: Option<usize>,
    /// Skip entries whose filename is not valid UTF-8 and mark the parent
    /// directory with `read_error` (prevents lossy remote paths).
    pub skip_lossy_names: bool,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            excluded: FxHashSet::default(),
            dedup_in_walker: true,
            max_pending_links: None,
            skip_lossy_names: false,
        }
    }
}

/// One item yielded by [`stream_walk`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StreamItem {
    /// Entering a directory (preorder).
    EnterDir {
        path: PathBuf,
        name: String,
        depth: usize,
        read_error: bool,
    },
    /// A leaf entry (file, symlink, or special node) inside the current dir.
    Entry {
        path: PathBuf,
        name: String,
        kind: NodeKind,
        allocated_bytes: u64,
        apparent_bytes: u64,
        modified: i64,
        hidden: bool,
        lossy_name: bool,
        hardlink: Option<(u64, u64)>,
    },
    /// Leaving a directory after all of its children have been emitted
    /// (postorder).
    LeaveDir {
        path: PathBuf,
        name: String,
        depth: usize,
        read_error: bool,
        direct_children: u64,
    },
    /// A non-fatal error reading a path.
    Error { path: PathBuf, message: String },
}

struct Frame {
    path: PathBuf,
    name: String,
    entries: Option<ReadDir>,
    read_error: bool,
    direct_children: u64,
}

/// Walk `root` iteratively with `O(depth)` memory, passing every [`StreamItem`]
/// to `emit`. Returning `Err` from `emit` aborts the walk immediately.
pub fn stream_walk<F>(
    root: &Path,
    options: &ScanOptions,
    cancel: Option<&Arc<AtomicBool>>,
    emit: F,
) -> io::Result<()>
where
    F: FnMut(StreamItem) -> io::Result<()>,
{
    let mut excluded: FxHashSet<PathBuf> = never_scanned(root);
    if options.one_filesystem
        && let Some(bounds) = strict_mount_boundaries_for(root)
    {
        excluded.extend(bounds);
    }
    let config = StreamConfig {
        excluded,
        ..StreamConfig::default()
    };
    stream_walk_with_config(root, options, &config, cancel, emit)
}

/// [`stream_walk`] with caller-supplied exclusions, hardlink policy, and
/// lossy-name handling (used by the headless SSH helper).
pub fn stream_walk_with_config<F>(
    root: &Path,
    options: &ScanOptions,
    config: &StreamConfig,
    cancel: Option<&Arc<AtomicBool>>,
    mut emit: F,
) -> io::Result<()>
where
    F: FnMut(StreamItem) -> io::Result<()>,
{
    let root_meta = fs::metadata(root)?;
    let root_dev = device_of(&root_meta);
    let excluded = &config.excluded;

    let root_name = root.file_name().map_or_else(
        || root.to_string_lossy().into_owned(),
        |n| n.to_string_lossy().into_owned(),
    );

    let root_entries = fs::read_dir(root);
    let root_err = root_entries.is_err();
    if let Err(err) = &root_entries {
        emit(StreamItem::Error {
            path: root.to_path_buf(),
            message: err.to_string(),
        })?;
    }
    emit(StreamItem::EnterDir {
        path: root.to_path_buf(),
        name: root_name.clone(),
        depth: 0,
        read_error: root_err,
    })?;

    let mut frames = vec![Frame {
        path: root.to_path_buf(),
        name: root_name,
        entries: root_entries.ok(),
        read_error: root_err,
        direct_children: 0,
    }];
    let mut visited_dirs: FxHashSet<(u64, u64)> = FxHashSet::default();
    if let Some(id) = inode_of(&root_meta) {
        visited_dirs.insert(id);
    }
    // Track remaining hardlink count for bounded-memory deduplication.
    let mut seen_links: FxHashMap<(u64, u64), u64> = FxHashMap::default();

    while !frames.is_empty() {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Ok(());
        }

        let current_depth = frames.len();
        let frame = frames.last_mut().expect("non-empty frames");
        let next_entry = frame.entries.as_mut().and_then(Iterator::next);
        let Some(entry_res) = next_entry else {
            let done = frames.pop().expect("frame checked above");
            let depth = frames.len();
            emit(StreamItem::LeaveDir {
                path: done.path,
                name: done.name,
                depth,
                read_error: done.read_error,
                direct_children: done.direct_children,
            })?;
            continue;
        };

        let entry = match entry_res {
            Ok(entry) => entry,
            Err(err) => {
                frame.read_error = true;
                emit(StreamItem::Error {
                    path: frame.path.clone(),
                    message: err.to_string(),
                })?;
                continue;
            }
        };

        let raw_name = entry.file_name();
        let lossy_name = raw_name.to_str().is_none();
        if lossy_name && config.skip_lossy_names {
            frame.read_error = true;
            continue;
        }
        let name = raw_name.to_string_lossy().into_owned();
        let hidden = name.starts_with('.');
        if !options.include_hidden && hidden {
            continue;
        }

        let path = entry.path();
        if excluded.contains(&path) {
            continue;
        }

        let sym_meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(err) => {
                frame.read_error = true;
                emit(StreamItem::Error {
                    path,
                    message: err.to_string(),
                })?;
                continue;
            }
        };

        let is_symlink = sym_meta.file_type().is_symlink();
        let meta = if is_symlink && options.follow_links {
            match fs::metadata(&path) {
                Ok(target) => target,
                Err(err) => {
                    if err.kind() != io::ErrorKind::NotFound {
                        emit(StreamItem::Error {
                            path,
                            message: err.to_string(),
                        })?;
                    }
                    continue;
                }
            }
        } else {
            sym_meta
        };

        if options.one_filesystem && device_of(&meta) != root_dev {
            continue;
        }

        if meta.is_dir() {
            if options.follow_links
                && let Some(id) = inode_of(&meta)
                && !visited_dirs.insert(id)
            {
                continue;
            }
            let next_depth = current_depth;
            if options.max_depth.is_some_and(|limit| next_depth > limit) {
                continue;
            }
            frame.direct_children += 1;
            let sub_entries = fs::read_dir(&path);
            let sub_err = sub_entries.is_err();
            if let Err(err) = &sub_entries {
                emit(StreamItem::Error {
                    path: path.clone(),
                    message: err.to_string(),
                })?;
            }
            emit(StreamItem::EnterDir {
                path: path.clone(),
                name: name.clone(),
                depth: next_depth,
                read_error: sub_err,
            })?;
            frames.push(Frame {
                path,
                name,
                entries: sub_entries.ok(),
                read_error: sub_err,
                direct_children: 0,
            });
            continue;
        }

        let kind = if meta.file_type().is_symlink() {
            NodeKind::Symlink
        } else if meta.is_file() {
            NodeKind::File
        } else {
            NodeKind::Other
        };

        let (mut alloc, mut apparent) = if kind == NodeKind::Other {
            (0, 0)
        } else {
            measure_both(&meta)
        };

        let hardlink = if (options.follow_links || nlink_of(&meta) > 1)
            && meta.is_file()
        {
            inode_of(&meta)
        } else {
            None
        };

        if options.dedup_hardlinks
            && config.dedup_in_walker
            && let Some(id) = hardlink
        {
            if let Some(remaining) = seen_links.get_mut(&id) {
                alloc = 0;
                apparent = 0;
                *remaining = remaining.saturating_sub(1);
                if *remaining == 0 {
                    seen_links.remove(&id);
                }
            } else {
                if let Some(limit) = config.max_pending_links
                    && seen_links.len() >= limit
                {
                    return Err(io::Error::other(format!(
                        "remote hardlink tracking limit reached at {}; scan a smaller root",
                        path.display()
                    )));
                }
                let extra = nlink_of(&meta).saturating_sub(1).max(1);
                seen_links.insert(id, extra);
            }
        }

        frame.direct_children += 1;
        emit(StreamItem::Entry {
            path,
            name,
            kind,
            allocated_bytes: alloc,
            apparent_bytes: apparent,
            modified: modified_seconds(&meta),
            hidden,
            lossy_name,
            hardlink,
        })?;
    }

    Ok(())
}

/// Measure `(allocated_bytes, apparent_bytes)` in a single `Metadata` pass.
#[cfg(unix)]
pub(crate) fn measure_both(meta: &Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt as _;
    (meta.blocks().saturating_mul(512), meta.len())
}

#[cfg(not(unix))]
pub(crate) fn measure_both(meta: &Metadata) -> (u64, u64) {
    let len = meta.len();
    (len, len)
}

#[cfg(unix)]
pub(crate) fn device_of(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.dev()
}

#[cfg(not(unix))]
pub(crate) const fn device_of(_meta: &Metadata) -> u64 {
    0
}

#[cfg(unix)]
pub(crate) fn inode_of(meta: &Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let ino = meta.ino();
    (ino != 0).then(|| (meta.dev(), ino))
}

#[cfg(not(unix))]
pub(crate) const fn inode_of(_meta: &Metadata) -> Option<(u64, u64)> {
    None
}

#[cfg(unix)]
pub(crate) fn nlink_of(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.nlink()
}

#[cfg(not(unix))]
pub(crate) const fn nlink_of(_meta: &Metadata) -> u64 {
    1
}

pub(crate) fn modified_seconds(meta: &Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs().cast_signed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_walk_dedups_hardlinks_and_emits_balanced_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let a = dir.path().join("a.bin");
        fs::write(&a, vec![42u8; 8192]).expect("write");
        fs::hard_link(&a, dir.path().join("b.bin")).expect("hardlink");
        fs::create_dir(dir.path().join("sub")).expect("mkdir");
        fs::write(dir.path().join("sub/c.txt"), b"hello").expect("write");

        let mut enters = 0usize;
        let mut leaves = 0usize;
        let mut total_apparent = 0u64;
        stream_walk(dir.path(), &ScanOptions::default(), None, |item| {
            match item {
                StreamItem::EnterDir { .. } => enters += 1,
                StreamItem::LeaveDir { .. } => leaves += 1,
                StreamItem::Entry { apparent_bytes, .. } => {
                    total_apparent += apparent_bytes;
                }
                StreamItem::Error { .. } => {}
            }
            Ok(())
        })
        .expect("stream");

        assert_eq!(enters, 2);
        assert_eq!(leaves, 2);
        assert_eq!(total_apparent, 8192 + 5);
    }
}

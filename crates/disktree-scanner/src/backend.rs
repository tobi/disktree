//! Strategy-invariant directory enumeration and metadata backend interface.
//!
//! Everything above this module (`walk`, `TreeArena`, checkpointing, SWR cache,
//! CLI, and GUI) operates solely on [`DirEntryMeta`] streams and never branches
//! on whether `io_uring`, synchronous `statx`, or portable `std::fs` produced
//! them. Adding a new OS or filesystem strategy only requires implementing one
//! backend behind [`visit_dir_entries`].

#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "linux")]
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::types::NodeKind;

/// Uniform metadata for a single directory entry, independent of the OS
/// strategy that read it.
#[derive(Clone, Copy, Debug)]
pub struct DirEntryMeta {
    pub kind: NodeKind,
    pub device: u64,
    pub allocated_bytes: u64,
    pub apparent_bytes: u64,
    pub modified: i64,
    pub hardlink: Option<(u64, u64)>,
    pub inode: Option<(u64, u64)>,
    pub hidden_attr: bool,
    pub evicted: bool,
}

/// Directory-enumeration / metadata strategy selector.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ScanBackend {
    /// Pick the fastest available strategy for the host (checks
    /// `DISKTREE_SCAN_BACKEND` if set; otherwise prefers `LinuxIoUring` when
    /// enabled and permitted on Linux, falling back to `LinuxStatx`).
    #[default]
    Auto,
    /// Linux `openat` + 64 KiB `getdents64` + relative `statx`.
    #[cfg(target_os = "linux")]
    LinuxStatx,
    /// Linux `openat` + 64 KiB `getdents64` + batched `io_uring`
    /// `IORING_OP_STATX` (automatically falls back to `LinuxStatx` if blocked).
    #[cfg(all(target_os = "linux", feature = "io-uring"))]
    LinuxIoUring,
    /// Portable `std::fs::read_dir` + `symlink_metadata` strategy.
    Portable,
}

impl ScanBackend {
    /// Resolve `Auto` via `DISKTREE_SCAN_BACKEND` (or the platform default).
    #[must_use]
    pub fn resolve(self) -> Self {
        if self != Self::Auto {
            return self;
        }
        if let Ok(raw) = std::env::var("DISKTREE_SCAN_BACKEND")
            && let Some(parsed) = Self::parse(&raw)
            && parsed != Self::Auto
        {
            return parsed;
        }
        Self::platform_default()
    }

    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "portable" | "std" => Some(Self::Portable),
            #[cfg(target_os = "linux")]
            "statx" | "linux-statx" => Some(Self::LinuxStatx),
            #[cfg(all(target_os = "linux", feature = "io-uring"))]
            "uring" | "io-uring" | "io_uring" | "linux-io-uring" => {
                Some(Self::LinuxIoUring)
            }
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            #[cfg(target_os = "linux")]
            Self::LinuxStatx => "linux-statx",
            #[cfg(all(target_os = "linux", feature = "io-uring"))]
            Self::LinuxIoUring => "linux-io-uring",
            Self::Portable => "portable",
        }
    }

    #[must_use]
    const fn platform_default() -> Self {
        #[cfg(all(target_os = "linux", feature = "io-uring"))]
        {
            Self::LinuxIoUring
        }
        #[cfg(all(target_os = "linux", not(feature = "io-uring")))]
        {
            Self::LinuxStatx
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::Portable
        }
    }
}

/// Enumerate and measure every non-`.` / non-`..` entry in `dir_path` using
/// `backend`.
///
/// - `should_skip(raw_name_bytes, is_dir_hint)` runs **before** issuing any
///   `stat` / `statx` syscall so cached fresh subtrees and mount exclusions
///   never touch the inode.
/// - `visit(raw_name_bytes, meta_result)` receives each unskipped entry;
///   returning `false` stops scanning the directory early (e.g. on cancel).
#[cfg(target_os = "linux")]
pub fn visit_dir_entries<S, V>(
    dir_path: &Path,
    follow_links: bool,
    backend: ScanBackend,
    should_skip: S,
    visit: V,
) -> io::Result<()>
where
    S: FnMut(&[u8], bool) -> bool,
    V: FnMut(&[u8], io::Result<DirEntryMeta>) -> bool,
{
    match backend.resolve() {
        #[cfg(feature = "io-uring")]
        ScanBackend::LinuxIoUring | ScanBackend::Auto => {
            crate::linux::visit_dir_uring(
                dir_path,
                follow_links,
                should_skip,
                visit,
            )
        }
        #[cfg(not(feature = "io-uring"))]
        ScanBackend::Auto => crate::linux::visit_dir_statx(
            dir_path,
            follow_links,
            should_skip,
            visit,
        ),
        ScanBackend::LinuxStatx => crate::linux::visit_dir_statx(
            dir_path,
            follow_links,
            should_skip,
            visit,
        ),
        ScanBackend::Portable => {
            visit_dir_portable(dir_path, follow_links, should_skip, visit)
        }
    }
}

#[cfg(target_os = "linux")]
fn visit_dir_portable<S, V>(
    dir_path: &Path,
    follow_links: bool,
    mut should_skip: S,
    mut visit: V,
) -> io::Result<()>
where
    S: FnMut(&[u8], bool) -> bool,
    V: FnMut(&[u8], io::Result<DirEntryMeta>) -> bool,
{
    use std::fs;
    use std::os::unix::ffi::OsStrExt as _;

    use crate::stream::{
        device_of, inode_of, measure_both, modified_seconds, nlink_of,
    };

    for entry_res in fs::read_dir(dir_path)? {
        let entry = match entry_res {
            Ok(e) => e,
            Err(err) => {
                if !visit(b"", Err(err)) {
                    break;
                }
                continue;
            }
        };
        let raw_name = entry.file_name();
        let raw_bytes = raw_name.as_bytes();
        let is_dir_hint = entry.file_type().is_ok_and(|ft| ft.is_dir());
        if should_skip(raw_bytes, is_dir_hint) {
            continue;
        }
        let path = entry.path();
        let meta_res = if follow_links {
            fs::metadata(&path)
        } else {
            fs::symlink_metadata(&path)
        };
        let mapped = meta_res.map(|meta| {
            let ft = meta.file_type();
            let kind = if ft.is_symlink() {
                NodeKind::Symlink
            } else if ft.is_dir() {
                NodeKind::Directory
            } else if ft.is_file() {
                NodeKind::File
            } else {
                NodeKind::Other
            };
            let (allocated_bytes, apparent_bytes) = measure_both(&meta);
            let inode = inode_of(&meta);
            let hardlink = (kind == NodeKind::File
                && (follow_links || nlink_of(&meta) > 1))
                .then_some(inode)
                .flatten();
            DirEntryMeta {
                kind,
                device: device_of(&meta),
                allocated_bytes,
                apparent_bytes,
                modified: modified_seconds(&meta),
                hardlink,
                inode,
                hidden_attr: false,
                evicted: false,
            }
        });
        if !visit(raw_bytes, mapped) {
            break;
        }
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::fs;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::types::ScanOptions;
    use crate::walk::{WalkConfig, WalkProgress, walk_parallel};

    #[test]
    fn all_linux_strategies_produce_identical_trees() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        fs::create_dir_all(root.join("a/b/c/d/e")).expect("mkdir");
        fs::create_dir_all(root.join(".hidden")).expect("mkdir hidden");
        for i in 0..40 {
            fs::write(
                root.join(format!("a/file-{i:02}.bin")),
                vec![1u8; 64 + i],
            )
            .expect("write");
        }
        fs::write(root.join("a/b/c/d/e/deep.txt"), b"hello deep")
            .expect("write deep");
        fs::write(root.join(".hidden/secret.txt"), b"hidden")
            .expect("write hidden");
        fs::hard_link(
            root.join("a/file-00.bin"),
            root.join("a/file-00-link.bin"),
        )
        .expect("hardlink");

        let run = |backend: ScanBackend| {
            let options = ScanOptions {
                backend,
                ..ScanOptions::default()
            };
            let outcome = walk_parallel(
                root,
                WalkConfig {
                    options: &options,
                    cancel: Arc::new(AtomicBool::new(false)),
                    progress: Arc::new(WalkProgress::default()),
                    pause_gate: None,
                    skip_oracle: None,
                    on_dir_start: None,
                    on_dir_tally: None,
                    on_dir_complete: None,
                },
            )
            .expect("walk");
            let r = outcome.arena.node(outcome.arena.root());
            (r.files, r.dirs, r.allocated_bytes, r.apparent_bytes)
        };

        let portable = run(ScanBackend::Portable);
        let statx = run(ScanBackend::LinuxStatx);
        assert_eq!(statx, portable, "statx must match portable");

        #[cfg(feature = "io-uring")]
        {
            let uring = run(ScanBackend::LinuxIoUring);
            assert_eq!(uring, portable, "io_uring must match portable");
        }
    }
}

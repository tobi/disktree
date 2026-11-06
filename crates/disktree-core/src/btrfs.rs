//! Unprivileged Btrfs accounting. FIEMAP identifies reflinked data, including
//! references outside the scan, without mounting anything or reading contents.
//!
//! Encoded mappings expose their logical length, not their compressed length.
//! They are charged once by extent start as an estimate, never as reclaimable
//! bytes. Shared extents are likewise excluded from the conservative deletion
//! estimate: FIEMAP cannot prove that a selection contains every reference.

#![allow(
    unsafe_code,
    reason = "FIEMAP and FS_INFO have no rustix wrapper; each ioctl documents \
              its buffer and ABI"
)]

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::Path;

use rustc_hash::FxHashMap;

use crate::scan::ScanProgress;
use crate::tree::{Node, NodeKind};

const BTRFS_MAGIC: u64 = 0x9123_683e;
const LAST: u32 = 1;
const ENCODED: u32 = 8;
const SHARED: u32 = 0x2000;
const UNWRITTEN: u32 = 0x800;
const BATCH: usize = 128;
// Unknown, delayed, inline, tail-packed, merged and future extent flags must
// not be interpreted as ordinary physical ranges.
const KNOWN: u32 = LAST | ENCODED | SHARED | UNWRITTEN;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct Extent {
    logical: u64,
    physical: u64,
    length: u64,
    reserved64: [u64; 2],
    flags: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct Map {
    start: u64,
    length: u64,
    flags: u32,
    mapped: u32,
    count: u32,
    reserved: u32,
    extents: [Extent; BATCH],
}

unsafe extern "C" {
    fn ioctl(
        fd: std::ffi::c_int,
        request: std::ffi::c_ulong,
        ...
    ) -> std::ffi::c_int;
}

/// A disjoint union. Inserting a partially overlapping reflink charges only
/// the new part, even when the files were split into different extents.
#[derive(Default)]
struct Ranges(BTreeMap<u64, u64>);

impl Ranges {
    fn insert(&mut self, start: u64, end: u64) -> u64 {
        let mut first = start;
        let mut last = end;
        let mut added = end - start;
        if let Some((&left, &right)) = self.0.range(..=start).next_back()
            && right >= start
        {
            added -= right.min(end).saturating_sub(start);
            first = left;
            last = last.max(right);
            self.0.remove(&left);
        }
        while let Some((&left, &right)) = self.0.range(first..=last).next() {
            added -= right.min(end).saturating_sub(left.max(start));
            last = last.max(right);
            self.0.remove(&left);
        }
        self.0.insert(first, last);
        added
    }
}

#[derive(Default)]
struct Volume {
    plain: Ranges,
    encoded: FxHashMap<u64, u64>,
}

impl Volume {
    fn charge(&mut self, extents: &[Extent], links: u64) -> (u64, u64, bool) {
        let mut bytes = 0_u64;
        let mut reclaimable = 0_u64;
        let mut estimated = false;
        for e in extents {
            if e.flags & ENCODED != 0 {
                // Encoded logical lengths are NOT physical address ranges.
                // Different compressed extents can appear to overlap. Only
                // equal starts identify the same compressed extent.
                let seen = self.encoded.entry(e.physical).or_default();
                bytes = bytes.saturating_add(e.length.saturating_sub(*seen));
                *seen = (*seen).max(e.length);
                estimated = true;
            } else {
                let added =
                    self.plain.insert(e.physical, e.physical + e.length);
                bytes = bytes.saturating_add(added);
                if e.flags & SHARED == 0 && links == 1 {
                    reclaimable = reclaimable.saturating_add(added);
                }
            }
        }
        (bytes, reclaimable, estimated)
    }
}

#[derive(Default)]
struct Accountant {
    // FS_INFO's UUID, not st_dev: different Btrfs subvolumes share extents.
    volumes: FxHashMap<[u8; 16], Volume>,
    devices: FxHashMap<u64, Option<[u8; 16]>>,
}

/// Re-measure completed local leaves before their totals are aggregated.
/// Sorting by name makes allocation to the first referencing file stable.
/// A rescan constructs a new accountant; no physical addresses outlive it.
pub fn measure(
    root: &Path,
    node: &mut Node,
    progress: &ScanProgress,
    options: &crate::scan::ScanOptions,
) -> io::Result<()> {
    let btrfs = rustix::fs::statfs(root)?.f_type as u64 == BTRFS_MAGIC;
    if options.one_filesystem && !btrfs {
        return Ok(());
    }
    progress
        .accounting
        .store(true, std::sync::atomic::Ordering::Relaxed);
    Accountant::default().visit(
        root,
        node,
        progress,
        btrfs,
        options.apparent_size,
    )
}

impl Accountant {
    fn volume(
        &mut self,
        file: &File,
        device: u64,
    ) -> io::Result<Option<[u8; 16]>> {
        if let Some(id) = self.devices.get(&device) {
            return Ok(*id);
        }
        let stat = rustix::fs::fstatfs(file)?;
        let id = if stat.f_type as u64 == BTRFS_MAGIC {
            // FS_INFO is a fixed 1024-byte UAPI buffer; UUID is bytes 16..32.
            let mut info = [0_u64; 128];
            // SAFETY: the kernel writes at most 1024 aligned, initialized
            // bytes into this fixed-size buffer.
            if unsafe {
                ioctl(
                    file.as_raw_fd(),
                    rustix::ioctl::opcode::read::<[u64; 128]>(0x94, 31).into(),
                    info.as_mut_ptr(),
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut id = [0; 16];
            id[..8].copy_from_slice(&info[2].to_ne_bytes());
            id[8..].copy_from_slice(&info[3].to_ne_bytes());
            Some(id)
        } else {
            None
        };
        self.devices.insert(device, id);
        Ok(id)
    }

    fn visit(
        &mut self,
        path: &Path,
        node: &mut Node,
        progress: &ScanProgress,
        inherited: bool,
        apparent: bool,
    ) -> io::Result<()> {
        if progress.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Btrfs accounting cancelled",
            ));
        }
        // Nodes use lossy UTF-8 names for display. A replacement character
        // may stand for a different on-disk name; do not measure its alias.
        if node.name.contains('\u{fffd}') {
            if inherited {
                unknown(node, apparent);
            }
            return Ok(());
        }
        if node.is_dir() {
            // Do not follow a directory replaced with a symlink since the walk.
            let volume = open(path)
                .and_then(|file| self.volume(&file, file.metadata()?.dev()));
            if volume.is_err() {
                if inherited {
                    unknown(node, apparent);
                }
                return Ok(());
            }
            let btrfs = matches!(volume, Ok(Some(_)));
            if btrfs {
                node.set_reclaimable(0);
            }
            node.children.sort_unstable_by(|a, b| a.name.cmp(&b.name));
            for child in &mut node.children {
                self.visit(
                    &path.join(&*child.name),
                    child,
                    progress,
                    btrfs,
                    apparent,
                )?;
            }
            return Ok(());
        }
        if node.kind != NodeKind::File {
            if inherited {
                node.set_reclaimable(0);
            }
            return Ok(());
        }
        // Check the parent first: non-Btrfs files need no open or extent query.
        if !inherited {
            return Ok(());
        }
        // On any failure retain the ordinary size, explicitly estimated, and
        // promise no saving. Opening nonblocking avoids a replaced FIFO hang.
        node.set_reclaimable(0);
        node.size_estimated = !apparent;
        let Ok(file) = open(path) else {
            return Ok(());
        };
        let Ok(before) = file.metadata() else {
            return Ok(());
        };
        if !before.is_file() {
            return Ok(());
        }
        let Ok(Some(id)) = self.volume(&file, before.dev()) else {
            return Ok(());
        };
        let Ok(extents) = mappings(&file, progress) else {
            if progress.is_cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "Btrfs accounting cancelled",
                ));
            }
            return Ok(());
        };
        let Ok(after) = file.metadata() else {
            return Ok(());
        };
        if before.len() != after.len()
            || before.mtime_nsec() != after.mtime_nsec()
            || before.mtime() != after.mtime()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
        {
            return Ok(());
        }
        // An unsupported mapping invalidates the whole file, before committing
        // any ranges: otherwise a failed file could hide bytes in later files.
        if extents.iter().any(|e| {
            e.flags & !KNOWN != 0
                || e.physical == 0
                || e.length == 0
                || e.physical.checked_add(e.length).is_none()
        }) {
            return Ok(());
        }
        let volume = self.volumes.entry(id).or_default();
        let (bytes, reclaimable, estimated) =
            volume.charge(&extents, before.nlink());
        if !apparent {
            node.bytes = bytes;
        }
        node.set_reclaimable(reclaimable);
        node.size_estimated = estimated && !apparent;
        Ok(())
    }
}

fn unknown(node: &mut Node, apparent: bool) {
    node.set_reclaimable(0);
    node.size_estimated = !apparent;
    for child in &mut node.children {
        unknown(child, apparent);
    }
}

fn open(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(
            i32::try_from(
                (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK)
                    .bits(),
            )
            .expect("open flags fit a C int"),
        )
        .open(path)
}

fn mappings(file: &File, progress: &ScanProgress) -> io::Result<Vec<Extent>> {
    let mut result = Vec::new();
    let mut start = 0;
    loop {
        if progress.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "cancelled",
            ));
        }
        let mut map = Map {
            start,
            length: u64::MAX - start,
            // Do not force writeback of the user's files just to measure them.
            flags: 0,
            mapped: 0,
            count: BATCH as u32,
            reserved: 0,
            extents: [Extent::default(); BATCH],
        };
        // SAFETY: repr(C) header is 32 bytes, followed by count 56-byte
        // extents. The ioctl size is the header size, not the array size.
        if unsafe {
            ioctl(
                file.as_raw_fd(),
                rustix::ioctl::opcode::read_write::<[u64; 4]>(0x66, 11).into(),
                &raw mut map,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let count = map.mapped as usize;
        if count > BATCH {
            return Err(io::Error::other("invalid FIEMAP count"));
        }
        let Some(last) = map.extents[..count].last() else {
            return Ok(result);
        };
        let next = last
            .logical
            .checked_add(last.length)
            .filter(|next| *next > start)
            .ok_or_else(|| io::Error::other("FIEMAP did not advance"))?;
        if result.len() > 1_000_000 {
            return Err(io::Error::other("FIEMAP extent limit exceeded"));
        }
        result.extend_from_slice(&map.extents[..count]);
        if last.flags & LAST != 0 {
            return Ok(result);
        }
        start = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_layout_matches_linux_uapi() {
        assert_eq!(size_of::<Extent>(), 56);
        assert_eq!(std::mem::offset_of!(Map, extents), 32);
        assert_eq!(size_of::<Map>(), 32 + BATCH * 56);
    }

    #[test]
    fn partial_reflinks_and_bridging_ranges_are_counted_once() {
        let mut ranges = Ranges::default();
        assert_eq!(ranges.insert(100, 200), 100);
        assert_eq!(ranges.insert(150, 250), 50);
        assert_eq!(ranges.insert(300, 400), 100);
        assert_eq!(ranges.insert(50, 450), 150);
        assert_eq!(ranges.insert(100, 200), 0);
        assert_eq!(ranges.0.into_iter().collect::<Vec<_>>(), vec![(50, 450)]);
    }

    fn extent(physical: u64, length: u64, flags: u32) -> Extent {
        Extent {
            physical,
            length,
            flags,
            ..Extent::default()
        }
    }

    #[test]
    fn encoded_extents_do_not_overlap_by_their_logical_lengths() {
        let mut volume = Volume::default();
        assert_eq!(
            volume.charge(&[extent(100, 1000, ENCODED)], 1),
            (1000, 0, true)
        );
        assert_eq!(
            volume.charge(&[extent(200, 1000, ENCODED)], 1),
            (1000, 0, true)
        );
        assert_eq!(
            volume.charge(&[extent(100, 1000, ENCODED)], 1),
            (0, 0, true)
        );
        assert_eq!(
            volume.charge(&[extent(100, 1500, ENCODED)], 1),
            (500, 0, true)
        );
    }

    #[test]
    fn exclusive_data_is_separate_from_shared_and_hardlinked_data() {
        let mut volume = Volume::default();
        let extents = [extent(100, 100, SHARED), extent(200, 100, 0)];
        assert_eq!(volume.charge(&extents, 1), (200, 100, false));
        assert_eq!(Volume::default().charge(&extents, 2), (200, 0, false));
        // Another filesystem's identical addresses must not be deduplicated.
        assert_eq!(Volume::default().charge(&extents, 1), (200, 100, false));
    }

    #[test]
    fn cancelled_accounting_does_not_publish_a_partial_result() {
        let temp = tempfile::tempdir().unwrap();
        let progress = ScanProgress::default();
        progress.cancel();
        let mut node = Node::directory("root");
        let error = Accountant::default()
            .visit(temp.path(), &mut node, &progress, true, false)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn live_compression_is_labelled_and_sparse_holes_are_not_charged() {
        let Some(parent) = std::env::var_os("DISKTREE_BTRFS_TEST_DIR") else {
            return;
        };
        let temp = tempfile::tempdir_in(parent).unwrap();
        let compressed = temp.path().join("compressed");
        std::fs::write(&compressed, vec![b'x'; 1024 * 1024]).unwrap();
        File::open(&compressed).unwrap().sync_all().unwrap();
        let sparse = File::create(temp.path().join("sparse")).unwrap();
        sparse.set_len(32 * 1024 * 1024).unwrap();
        sparse.sync_all().unwrap();
        let tree =
            crate::scan::scan(temp.path(), crate::scan::ScanOptions::default())
                .unwrap();
        assert_eq!(tree.child_named("sparse").unwrap().bytes, 0);
        let extents = mappings(
            &File::open(compressed).unwrap(),
            &ScanProgress::default(),
        )
        .unwrap();
        if extents.iter().any(|extent| extent.flags & ENCODED != 0) {
            assert!(tree.size_estimated);
            assert_eq!(tree.reclaimable(), Some(0));
        }
    }

    #[test]
    fn live_unreadable_files_keep_estimated_size_without_a_saving() {
        use std::os::unix::fs::PermissionsExt as _;
        let Some(parent) = std::env::var_os("DISKTREE_BTRFS_TEST_DIR") else {
            return;
        };
        let temp = tempfile::tempdir_in(parent).unwrap();
        let file = temp.path().join("unreadable");
        std::fs::write(&file, vec![b'x'; 1024 * 1024]).unwrap();
        File::open(&file).unwrap().sync_all().unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000))
            .unwrap();
        // Running tests as root defeats the unreadable-file fixture.
        if File::open(&file).is_ok() {
            return;
        }
        let tree =
            crate::scan::scan(temp.path(), crate::scan::ScanOptions::default())
                .unwrap();
        assert!(tree.bytes > 0);
        assert!(tree.size_estimated);
        assert_eq!(tree.reclaimable(), Some(0));
    }

    /// Set `DISKTREE_BTRFS_TEST_DIR` to a writable Btrfs directory. No mounts,
    /// sudo, or changes to existing files are required.
    #[test]
    fn live_reflinks_count_once_and_do_not_promise_shared_space() {
        let Some(parent) = std::env::var_os("DISKTREE_BTRFS_TEST_DIR") else {
            return;
        };
        let temp = tempfile::tempdir_in(parent).unwrap();
        let original = temp.path().join("a");
        // Deterministic noise prevents transparent compression in this fixture.
        let mut seed = 42_u64;
        let data: Vec<u8> = (0..1024 * 1024)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed as u8
            })
            .collect();
        std::fs::write(&original, data).unwrap();
        File::open(&original).unwrap().sync_all().unwrap();
        for name in ["b", "c"] {
            assert!(
                std::process::Command::new("cp")
                    .arg("--reflink=always")
                    .arg(&original)
                    .arg(temp.path().join(name))
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let tree =
            crate::scan::scan(temp.path(), crate::scan::ScanOptions::default())
                .unwrap();
        assert_eq!(tree.bytes, 1024 * 1024);
        assert_eq!(tree.reclaimable(), Some(0));
        assert!(!tree.size_estimated);
        assert_eq!(tree.child_named("a").unwrap().bytes, 1024 * 1024);
        assert_eq!(tree.child_named("b").unwrap().bytes, 0);
        // Both apparent-size mode and partial selections keep sharing separate
        // from the layout weights. An alias outside the scanned root still
        // prevents an exclusive-space claim through FIEMAP_EXTENT_SHARED.
        let apparent = crate::scan::scan(
            temp.path(),
            crate::scan::ScanOptions {
                apparent_size: true,
                ..crate::scan::ScanOptions::default()
            },
        )
        .unwrap();
        assert_eq!(apparent.bytes, 3 * 1024 * 1024);
        assert_eq!(apparent.reclaimable(), Some(0));
        let target = crate::removal::Target {
            path: original.clone(),
            bytes: tree.child_named("a").unwrap().bytes,
            is_dir: false,
            hidden: false,
        };
        let mut plan = crate::removal::plan(&[target], temp.path());
        plan.account_storage(&tree, crate::removal::RemovalMode::Permanent);
        assert_eq!(plan.reclaim(), 0);
        assert!(plan.btrfs_estimate);
        plan.account_storage(&tree, crate::removal::RemovalMode::Permanent);
        assert_eq!(plan.reclaim(), 0, "projection is idempotent");

        std::fs::remove_file(temp.path().join("b")).unwrap();
        std::fs::remove_file(temp.path().join("c")).unwrap();
        File::open(&original).unwrap().sync_all().unwrap();
        let tree =
            crate::scan::scan(temp.path(), crate::scan::ScanOptions::default())
                .unwrap();
        assert_eq!(tree.bytes, 1024 * 1024);
        assert_eq!(tree.reclaimable(), Some(1024 * 1024));
        let target = crate::removal::Target {
            path: original,
            bytes: tree.bytes,
            is_dir: false,
            hidden: false,
        };
        let mut plan = crate::removal::plan(&[target], temp.path());
        plan.account_storage(&tree, crate::removal::RemovalMode::Trash);
        assert_eq!(plan.reclaim(), 0);
        assert_eq!(plan.unattributed(), 0);
        assert!(plan.deferred_to_trash);
    }
}

//! Volume capacity, mount boundaries, and cache free-space guards.

use std::io;
use std::path::{Path, PathBuf};

use rustc_hash::FxHashSet;

use crate::types::{FallbackReason, SpaceInfo};

/// Read capacity of the volume containing `path`.
#[cfg(not(windows))]
pub fn space_info(path: &Path) -> io::Result<SpaceInfo> {
    let stat = rustix::fs::statvfs(path)?;
    let block = if stat.f_frsize == 0 {
        stat.f_bsize
    } else {
        stat.f_frsize
    };
    Ok(SpaceInfo {
        total: stat.f_blocks.saturating_mul(block),
        free: stat.f_bfree.saturating_mul(block),
        available: stat.f_bavail.saturating_mul(block),
    })
}

#[cfg(windows)]
pub fn space_info(path: &Path) -> io::Result<SpaceInfo> {
    crate::windows::space_info(path)
}

/// Check whether `dir` (or its nearest existing ancestor) has at least
/// `required_headroom` bytes available for cache DB storage.
pub fn check_cache_headroom(
    path: &Path,
    required_headroom: u64,
) -> Result<(), FallbackReason> {
    if required_headroom == 0 {
        return Ok(());
    }
    let probe = nearest_existing_ancestor(path);
    let Ok(info) = space_info(&probe) else {
        // If statvfs isn't available on this target, let the DB open attempt
        // determine writability.
        return Ok(());
    };
    if info.available < required_headroom {
        return Err(FallbackReason::LowDiskSpace {
            available: info.available,
            required: required_headroom,
        });
    }
    Ok(())
}

/// Nearest ancestor of `path` that currently exists on disk.
pub fn nearest_existing_ancestor(path: &Path) -> PathBuf {
    let mut cur = path.to_path_buf();
    while !cur.exists() {
        if !cur.pop() {
            return PathBuf::from("/");
        }
    }
    cur
}

/// Default cache database file path for the current user.
pub fn default_cache_db_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME")?;
        return Some(
            PathBuf::from(home).join("Library/Caches/disktree/scan-cache.redb"),
        );
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("LOCALAPPDATA")?;
        return Some(
            PathBuf::from(base).join("disktree/cache/scan-cache.redb"),
        );
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME")
            && !xdg.is_empty()
        {
            return Some(PathBuf::from(xdg).join("disktree/scan-cache.redb"));
        }
        let home = std::env::var_os("HOME")?;
        Some(PathBuf::from(home).join(".cache/disktree/scan-cache.redb"))
    }
}

/// Every nested mount strictly beneath `root`, including bind mounts and
/// same-device Btrfs subvolumes. Used by remote/streaming walks.
pub fn strict_mount_boundaries_for(root: &Path) -> Option<FxHashSet<PathBuf>> {
    #[cfg(target_os = "linux")]
    {
        let table = std::fs::read_to_string("/proc/self/mounts").ok()?;
        let canonical =
            root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        Some(mount_boundaries_in(&table, &canonical))
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("mount").output().ok()?;
        if !output.status.success() {
            return None;
        }
        let canonical =
            root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        Some(
            parse_macos_mounts(&String::from_utf8_lossy(&output.stdout))
                .into_iter()
                .filter(|point| {
                    point != &canonical && point.starts_with(&canonical)
                })
                .collect(),
        )
    }
    #[cfg(windows)]
    {
        let canonical =
            root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        Some(
            crate::windows::mount_points()
                .into_iter()
                .filter(|point| {
                    point != &canonical && point.starts_with(&canonical)
                })
                .collect(),
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = root;
        None
    }
}

/// Mount points a local `--one-filesystem` scan must not enter.
///
/// Distinct Btrfs subvolumes on the same local device are included, while
/// duplicate views, snapshots, other filesystems and automounts stay out.
pub fn mount_boundaries_for(root: &Path) -> Option<FxHashSet<PathBuf>> {
    #[cfg(target_os = "linux")]
    {
        let table = std::fs::read_to_string("/proc/self/mounts").ok()?;
        let canonical =
            root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let boundaries = mount_boundaries_in(&table, &canonical);
        // Without mountinfo, don't guess whether a mount is a distinct
        // subvolume or a bind mount.
        let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
            return Some(boundaries);
        };
        let views: Vec<_> = info.lines().filter_map(MountView::parse).collect();
        Some(local_boundaries(boundaries, &views, &canonical))
    }
    #[cfg(not(target_os = "linux"))]
    {
        strict_mount_boundaries_for(root)
    }
}

#[cfg(target_os = "macos")]
fn parse_macos_mounts(output: &str) -> Vec<PathBuf> {
    output
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once(" on ")?;
            let (point, _) = rest.rsplit_once(" (")?;
            Some(PathBuf::from(point))
        })
        .collect()
}

/// Parse mount boundaries strictly below `root` from a `/proc/self/mounts`
/// formatted table.
pub fn mount_boundaries_in(table: &str, root: &Path) -> FxHashSet<PathBuf> {
    let mut boundaries = FxHashSet::default();
    for line in table.lines() {
        let mut fields = line.split_whitespace();
        let Some(_device) = fields.next() else {
            continue;
        };
        let Some(raw_point) = fields.next() else {
            continue;
        };
        let point = PathBuf::from(unescape_octal(raw_point));
        if point != root && point.starts_with(root) {
            boundaries.insert(point);
        }
    }
    boundaries
}

/// Remove distinct same-device Btrfs subvolumes from strict boundaries.
/// Overlapping filesystem roots are left out to avoid counting a bind mount
/// or another view of the same data twice.
pub fn local_boundaries<S: std::hash::BuildHasher>(
    mut boundaries: std::collections::HashSet<PathBuf, S>,
    views: &[MountView],
    root: &Path,
) -> std::collections::HashSet<PathBuf, S> {
    let Some(own) = views
        .iter()
        .filter(|view| root.starts_with(&view.point))
        .max_by_key(|view| view.point.components().count())
    else {
        return boundaries;
    };
    if own.fstype != "btrfs" || !own.source.starts_with("/dev/") {
        return boundaries;
    }
    let mut candidates: Vec<_> = views
        .iter()
        .filter(|view| boundaries.contains(&view.point))
        .collect();
    candidates.sort_by_key(|view| {
        (
            view.point.components().count(),
            view.point.as_os_str().len(),
            &view.point,
        )
    });
    let mut kept = vec![own.fs_root.clone()];
    for view in candidates {
        let unique = views
            .iter()
            .filter(|other| other.point == view.point)
            .count()
            == 1;
        if unique
            && view.fstype == "btrfs"
            && view.source == own.source
            && view.fs_root.is_absolute()
            && !view.snapshot
            && !kept.iter().any(|existing| {
                view.fs_root.starts_with(existing)
                    || existing.starts_with(&view.fs_root)
            })
        {
            kept.push(view.fs_root.clone());
            boundaries.remove(&view.point);
        }
    }
    boundaries
}

/// A mounted view needed to distinguish Btrfs subvolumes from aliases.
#[derive(Clone, Debug)]
pub struct MountView {
    pub point: PathBuf,
    pub fs_root: PathBuf,
    pub fstype: String,
    pub source: String,
    pub snapshot: bool,
}

impl MountView {
    fn parse(line: &str) -> Option<Self> {
        let (left, right) = line.split_once(" - ")?;
        let mut left = left.split_whitespace().skip(3);
        let fs_root = PathBuf::from(unescape_octal(left.next()?));
        let point = PathBuf::from(unescape_octal(left.next()?));
        let mut right = right.split_whitespace();
        let fstype = right.next()?.to_owned();
        let source = unescape_octal(right.next()?);
        let snapshot =
            point.file_name().is_some_and(|name| name == ".snapshots")
                || fs_root.to_string_lossy().contains("snapshots");
        Some(Self {
            point,
            fs_root,
            fstype,
            source,
            snapshot,
        })
    }
}

/// Directories under `root` a scan must never enter, even with
/// `one_filesystem = false`: virtual kernel filesystems on Linux, and macOS's
/// duplicate Data volume mount + `/Network` automount.
pub fn never_scanned(root: &Path) -> FxHashSet<PathBuf> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut set = FxHashSet::default();
        #[cfg(target_os = "linux")]
        for virtual_fs in ["/proc", "/sys", "/dev"] {
            let path = PathBuf::from(virtual_fs);
            if path != root && path.starts_with(root) {
                set.insert(path);
            }
        }
        #[cfg(target_os = "macos")]
        for skip in ["/System/Volumes/Data", "/Network"] {
            let path = PathBuf::from(skip);
            if path != root && path.starts_with(root) {
                set.insert(path);
            }
        }
        set
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = root;
        FxHashSet::default()
    }
}

fn unescape_octal(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;
    while idx < bytes.len() {
        if bytes[idx] == b'\\'
            && idx + 3 < bytes.len()
            && bytes[idx + 1..idx + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let octal = ((bytes[idx + 1] - b'0') << 6)
                | ((bytes[idx + 2] - b'0') << 3)
                | (bytes[idx + 3] - b'0');
            out.push(octal);
            idx += 4;
        } else {
            out.push(bytes[idx]);
            idx += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn btrfs_home_is_included_without_entering_network_or_aliases() {
        let mounts = "\
/dev/root / btrfs rw 0 0\n\
/dev/root /home-copy btrfs rw 0 0\n\
/dev/root /home btrfs rw 0 0\n\
/dev/root /bind btrfs rw 0 0\n\
/dev/root /.snapshots btrfs rw 0 0\n\
/dev/other /external btrfs rw 0 0\n\
server:/share /nas nfs rw 0 0\n\
user@host:/ /ssh fuse.sshfs rw 0 0\n";
        let info = "\
1 0 0:1 /@ / rw - btrfs /dev/root rw\n\
2 1 0:1 /@home /home-copy rw - btrfs /dev/root rw\n\
3 1 0:1 /@home /home rw - btrfs /dev/root rw\n\
4 1 0:1 /@/data /bind rw - btrfs /dev/root rw\n\
5 1 0:1 /@snapshots /.snapshots rw - btrfs /dev/root rw\n\
6 1 0:2 /@data /external rw - btrfs /dev/other rw\n\
7 1 0:3 / /nas rw - nfs server:/share rw\n\
8 1 0:4 / /ssh rw - fuse.sshfs user@host:/ rw\n";
        let views: Vec<_> = info.lines().filter_map(MountView::parse).collect();
        let boundaries = local_boundaries(
            mount_boundaries_in(mounts, Path::new("/")),
            &views,
            Path::new("/"),
        );
        assert!(!boundaries.contains(Path::new("/home")));
        for path in [
            "/home-copy",
            "/bind",
            "/.snapshots",
            "/external",
            "/nas",
            "/ssh",
        ] {
            assert!(boundaries.contains(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn mount_boundaries_exclude_nested_and_bind_mounts() {
        let table = "\
/dev/nvme0n1p2 / ext4 rw 0 0
/dev/nvme0n1p2 /home/tobi/bind\\040mount ext4 rw 0 0
tmpfs /tmp tmpfs rw 0 0
";
        let boundaries = mount_boundaries_in(table, Path::new("/home/tobi"));
        assert!(boundaries.contains(Path::new("/home/tobi/bind mount")));
        assert!(!boundaries.contains(Path::new("/tmp")));
    }

    #[test]
    fn headroom_guard_flags_impossible_requirement() {
        let dir = tempfile::tempdir().expect("tempdir");
        if space_info(dir.path()).is_ok() {
            let result = check_cache_headroom(dir.path(), u64::MAX);
            assert!(matches!(result, Err(FallbackReason::LowDiskSpace { .. })));
        }
    }
}

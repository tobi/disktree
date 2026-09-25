//! Free space on the volume a path lives on.
//!
//! This is what makes "amount of crap I found" a real number rather than an
//! estimate: the app measures the volume before and after a removal, and shows
//! the projection while marking.

use std::io;
use std::path::{Path, PathBuf};

#[cfg(target_os = "macos")]
mod macos;

/// A volume's capacity in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpaceInfo {
    /// Total size of the volume.
    pub total: u64,
    /// Free blocks, including the reserve only root may write to.
    pub free: u64,
    /// Free blocks this user may actually write; what `df -h` reports.
    pub available: u64,
}

impl SpaceInfo {
    /// Space in use, computed from `free` rather than `available` so the figure
    /// does not jump when a reserve is opened to root.
    pub const fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// Share of the volume in use, `0.0..=1.0`.
    pub fn used_fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.used() as f64 / self.total as f64) as f32
        }
    }

    /// Free space after `bytes` are removed, saturating at the volume size and
    /// never counting the same byte twice.
    #[must_use]
    pub fn after_removing(&self, bytes: u64) -> Self {
        let available = self.available.saturating_add(bytes).min(self.total);
        let free = self.free.saturating_add(bytes).min(self.total);
        Self {
            total: self.total,
            free,
            available,
        }
    }
}

/// Read the space on the volume containing `path`.
pub fn space_info(path: &Path) -> io::Result<SpaceInfo> {
    let stat = rustix::fs::statvfs(path)?;
    // `f_frsize` is the fragment size the block counts are expressed in;
    // `f_bsize` is only a hint for I/O. Some filesystems report zero for
    // `f_frsize`, so fall back rather than claiming a zero-sized volume.
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

/// The device a path's filesystem is mounted from, such as
/// `/dev/nvme0n1p2`: the mount with the longest prefix of `path` in
/// `/proc/self/mounts`. `None` where that table cannot be read.
pub fn device_for(path: &Path) -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        macos::device(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let table = std::fs::read_to_string("/proc/self/mounts").ok()?;
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        device_in(&table, &path)
    }
}

/// [`device_for`] over a given mount table, for testing.
pub fn device_in(table: &str, path: &Path) -> Option<String> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let device = fields.next()?;
            // Spaces in mount points are escaped as \040.
            let mount = fields.next()?.replace("\\040", " ");
            path.starts_with(&mount)
                .then(|| (mount.len(), device.to_string()))
        })
        .max_by_key(|(length, _)| *length)
        .map(|(_, device)| device)
}

/// One line of the mount table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    /// What is mounted: a device, or a name like `tmpfs` or `systemd-1`.
    pub source: String,
    pub point: PathBuf,
    pub fstype: String,
    pub options: String,
}

/// Parse `/proc/self/mounts`.
pub fn parse_mounts(table: &str) -> Vec<Mount> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = fields.next()?.to_string();
            // Spaces in mount points are escaped as \040.
            let point = PathBuf::from(fields.next()?.replace("\\040", " "));
            let fstype = fields.next()?.to_string();
            let options = fields.next().unwrap_or_default().to_string();
            Some(Mount {
                source,
                point,
                fstype,
                options,
            })
        })
        .collect()
}

/// Mount points below `root` that are not part of `root`'s volume, which a
/// scan of that volume must not enter.
///
/// "The volume" is the mount *source*, not the device number: btrfs gives
/// every subvolume its own `st_dev`, so `/home` on Omarchy looks like a
/// different filesystem from `/` though it is the same disk. Everything
/// else is left out: pseudo filesystems (`/proc`, `/sys`), tmpfs, other
/// disks, network shares, and automount points — entering one of those
/// would mount a NAS just to measure it. Snapshot subvolumes are left out
/// too, because every file in them shares its blocks with the live one and
/// counting them would count the disk twice.
pub fn foreign_mounts(mounts: &[Mount], root: &Path) -> Vec<PathBuf> {
    let Some(own) = mounts
        .iter()
        .filter(|mount| root.starts_with(&mount.point))
        .max_by_key(|mount| mount.point.as_os_str().len())
    else {
        return Vec::new();
    };
    mounts
        .iter()
        .filter(|mount| mount.point != root && mount.point.starts_with(root))
        .filter(|mount| {
            mount.source != own.source
                || mount.fstype != own.fstype
                || is_snapshot(mount)
        })
        .map(|mount| mount.point.clone())
        .collect()
}

fn is_snapshot(mount: &Mount) -> bool {
    let named = mount
        .point
        .file_name()
        .is_some_and(|name| name == ".snapshots");
    named
        || mount.options.split(',').any(|option| {
            option.starts_with("subvol=") && option.contains("snapshots")
        })
}

/// The top of the disk `path` lives on.
///
/// The shortest mount point above it with the same source. On Omarchy the home directory is the `@home`
/// subvolume at `/home`, and the disk is `/`; with a separate home disk it
/// would be `/home`.
pub fn volume_root(mounts: &[Mount], path: &Path) -> Option<PathBuf> {
    let own = mounts
        .iter()
        .filter(|mount| path.starts_with(&mount.point))
        .max_by_key(|mount| mount.point.as_os_str().len())?;
    mounts
        .iter()
        .filter(|mount| {
            mount.source == own.source
                && mount.fstype == own.fstype
                && path.starts_with(&mount.point)
        })
        .min_by_key(|mount| mount.point.as_os_str().len())
        .map(|mount| mount.point.clone())
}

/// [`volume_root`] for this machine.
pub fn volume_root_for(path: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        macos::volume_root(path)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let table = std::fs::read_to_string("/proc/self/mounts").ok()?;
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        volume_root(&parse_mounts(&table), &path)
    }
}

/// [`foreign_mounts`] for this machine; `None` when the mount table cannot
/// be read, so the caller can fall back to comparing devices.
pub fn foreign_mounts_for(root: &Path) -> Option<Vec<PathBuf>> {
    #[cfg(target_os = "macos")]
    {
        macos::foreign_mounts(root)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let table = std::fs::read_to_string("/proc/self/mounts").ok()?;
        Some(foreign_mounts(&parse_mounts(&table), root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OMARCHY: &str = "\
sys /sys sysfs rw 0 0
run /run tmpfs rw 0 0
/dev/mapper/root / btrfs rw,subvolid=256,subvol=/@ 0 0
/dev/mapper/root /home btrfs rw,subvolid=257,subvol=/@home 0 0
/dev/mapper/root /var/log btrfs rw,subvolid=258,subvol=/@log 0 0
/dev/mapper/root /.snapshots btrfs rw,subvolid=260,subvol=/@snapshots 0 0
/dev/nvme0n1p1 /boot vfat rw 0 0
systemd-1 /mnt/nas-home autofs rw,direct 0 0
tmpfs /tmp tmpfs rw 0 0
portal /run/user/1000/doc fuse.portal rw 0 0
";

    #[test]
    fn a_volume_includes_its_subvolumes_and_nothing_else() {
        let mounts = parse_mounts(OMARCHY);
        let mut foreign = foreign_mounts(&mounts, Path::new("/"));
        foreign.sort();
        let expected: Vec<PathBuf> = [
            "/.snapshots",
            "/boot",
            "/mnt/nas-home",
            "/run",
            "/run/user/1000/doc",
            "/sys",
            "/tmp",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(foreign, expected, "/home and /var/log are the same disk");
    }

    #[test]
    fn the_whole_disk_is_the_top_of_the_home_volume() {
        let mounts = parse_mounts(OMARCHY);
        let root = volume_root(&mounts, Path::new("/home/tobi"));
        assert_eq!(root, Some(PathBuf::from("/")), "@home is on the root disk");
        let separate = parse_mounts(
            "/dev/sda1 / ext4 rw 0 0\n/dev/sdb1 /home ext4 rw 0 0\n",
        );
        let root = volume_root(&separate, Path::new("/home/tobi"));
        assert_eq!(root, Some(PathBuf::from("/home")), "a separate home disk");
    }

    #[test]
    fn a_home_scan_has_no_foreign_mounts_here() {
        let mounts = parse_mounts(OMARCHY);
        assert!(foreign_mounts(&mounts, Path::new("/home/tobi")).is_empty());
    }

    #[test]
    fn the_device_is_the_longest_matching_mount() {
        let table = "\
/dev/nvme0n1p2 / btrfs rw 0 0
tmpfs /tmp tmpfs rw 0 0
/dev/sda1 /home/tobi/big\\040disk ext4 rw 0 0
";
        let device = |path: &str| device_in(table, Path::new(path));
        assert_eq!(device("/home/tobi").as_deref(), Some("/dev/nvme0n1p2"));
        assert_eq!(device("/tmp/x").as_deref(), Some("tmpfs"));
        assert_eq!(
            device("/home/tobi/big disk/a").as_deref(),
            Some("/dev/sda1"),
            "escaped spaces in mount points"
        );
    }

    #[test]
    fn a_real_volume_reports_plausible_numbers() {
        let temp = std::env::temp_dir();
        let space = space_info(&temp).expect("temp dir has a volume");
        assert!(space.total > 0, "{space:?}");
        assert!(space.free <= space.total, "{space:?}");
        assert!(space.available <= space.free, "{space:?}");
        assert!((0.0..=1.0).contains(&space.used_fraction()), "{space:?}");
    }

    #[test]
    fn a_missing_path_reports_the_io_error() {
        let error = space_info(Path::new("/definitely/not/here"))
            .expect_err("no volume");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn projecting_removal_cannot_exceed_the_volume() {
        let space = SpaceInfo {
            total: 1000,
            free: 100,
            available: 100,
        };
        let after = space.after_removing(50);
        assert_eq!(after.available, 150);
        assert_eq!(after.free, 150);
        assert_eq!(after.total, 1000);

        let capped = space.after_removing(10_000);
        assert_eq!(capped.available, 1000);
        assert_eq!(capped.used(), 0);
        assert!((capped.used_fraction() - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn used_fraction_handles_an_empty_volume_report() {
        let space = SpaceInfo::default();
        assert!((space.used_fraction() - 0.0).abs() < f32::EPSILON);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_volume_root_matches_home_filesystem_device() {
        let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
        let root = volume_root_for(&home).expect("home has a mounted volume");
        assert_eq!(device_for(&home), device_for(&root));
    }
}

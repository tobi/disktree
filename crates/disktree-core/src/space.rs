//! Free space on the volume a path lives on.
//!
//! This is what makes "amount of crap I found" a real number rather than an
//! estimate: the app measures the volume before and after a removal, and shows
//! the projection while marking.

use std::io;
use std::path::{Path, PathBuf};

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
#[cfg(windows)]
pub fn space_info(path: &Path) -> io::Result<SpaceInfo> {
    crate::windows::space_info(path)
}

/// Read the space on the volume containing `path`.
#[cfg(not(windows))]
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

/// The volume a path is on, as Windows names it: `C:\`, a share, or the
/// folder a volume is mounted on.
#[cfg(windows)]
pub fn device_for(path: &Path) -> Option<String> {
    crate::windows::volume_root(path).map(|root| root.display().to_string())
}

/// The device a path's filesystem is mounted from, such as
/// `/dev/nvme0n1p2`: the mount with the longest prefix of `path` in
/// `/proc/self/mounts`. `None` where that table cannot be read.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn device_for(path: &Path) -> Option<String> {
    let table = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    device_in(&table, &path)
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
#[cfg(not(any(target_os = "macos", windows)))]
pub fn volume_root_for(path: &Path) -> Option<PathBuf> {
    let table = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    volume_root(&parse_mounts(&table), &path)
}

/// The top of the volume `path` lives on: its drive, such as `C:\`.
#[cfg(windows)]
pub fn volume_root_for(path: &Path) -> Option<PathBuf> {
    crate::windows::volume_root(path)
}

/// [`foreign_mounts`] for this machine; `None` when the mount table cannot
/// be read, so the caller can fall back to comparing devices.
#[cfg(not(windows))]
pub fn foreign_mounts_for(root: &Path) -> Option<Vec<PathBuf>> {
    let table = std::fs::read_to_string("/proc/self/mounts").ok()?;
    Some(foreign_mounts(&parse_mounts(&table), root))
}

/// One line of `/proc/self/mountinfo`.
///
/// What [`Mount`] says, plus which directory of the filesystem the mount
/// shows. `/proc/self/mounts` leaves that out, and it is what tells a bind
/// mount from the filesystem itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountView {
    pub source: String,
    pub fstype: String,
    pub point: PathBuf,
    /// The directory inside the filesystem that appears at `point`: `/` for
    /// a whole filesystem, `/@home` for a Btrfs subvolume, the bound
    /// directory for a bind mount.
    pub fs_root: PathBuf,
}

/// Parse `/proc/self/mountinfo`. Lines it cannot read are skipped.
pub fn parse_mountinfo(table: &str) -> Vec<MountView> {
    table
        .lines()
        .filter_map(|line| {
            // Optional fields run up to a lone `-`; after it come the type
            // and the source.
            let (left, right) = line.split_once(" - ")?;
            let mut left = left.split_whitespace().skip(3);
            let fs_root = PathBuf::from(unescape_octal(left.next()?));
            let point = PathBuf::from(unescape_octal(left.next()?));
            let mut right = right.split_whitespace();
            let fstype = right.next()?.to_string();
            let source = unescape_octal(right.next()?);
            Some(MountView {
                source,
                fstype,
                point,
                fs_root,
            })
        })
        .collect()
}

/// The kernel writes space, tab, newline and backslash in mount fields as
/// three octal digits after a backslash. Decoded in one pass, so a name that
/// really contains `\040` is not decoded twice.
fn unescape_octal(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let digits = bytes.get(index + 1..index + 4);
        let value = digits
            .filter(|_| bytes[index] == b'\\')
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| u8::from_str_radix(digits, 8).ok());
        if let Some(value) = value {
            out.push(value);
            index += 4;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Paths under `root` that show a directory the scan already reaches
/// another way, so walking them would count the same files twice.
///
/// A bind mount, a second mount of the same disk, or the top of a Btrfs
/// filesystem mounted beside its subvolumes all show one directory at two
/// paths. Device numbers cannot tell: a bind mount has its original's. The
/// mount table can, since each line names the filesystem and the directory
/// of it that is shown. When one directory is visible twice, the view
/// showing the widest part of the filesystem is kept, and the narrower one
/// is left out. The scanned root itself is never left out; if it is the
/// narrower view, its copy inside the wider one is.
pub fn repeated_mounts(mounts: &[MountView], root: &Path) -> Vec<PathBuf> {
    let mut visible: Vec<&MountView> = mounts
        .iter()
        .filter(|mount| {
            mount.point.starts_with(root) || root.starts_with(&mount.point)
        })
        .collect();
    visible.sort_by_key(|mount| {
        (
            mount.fs_root.components().count(),
            mount.point.components().count(),
            &mount.point,
        )
    });
    let mut kept: Vec<&MountView> = Vec::new();
    let mut repeated = Vec::new();
    for mount in visible {
        // Where this view's directory already appears through a kept one.
        let elsewhere = kept.iter().find_map(|wider| {
            if wider.source != mount.source || wider.fstype != mount.fstype {
                return None;
            }
            let inside = mount.fs_root.strip_prefix(&wider.fs_root).ok()?;
            let path = wider.point.join(inside);
            (path.starts_with(root) && path != mount.point).then_some(path)
        });
        match elsewhere {
            Some(_) if mount.point != root && mount.point.starts_with(root) => {
                repeated.push(mount.point.clone());
            }
            Some(copy) => {
                if copy != root {
                    repeated.push(copy);
                }
                kept.push(mount);
            }
            None => kept.push(mount),
        }
    }
    repeated
}

/// [`repeated_mounts`] for this machine, in `root`'s own spelling. Empty
/// where there is no `/proc/self/mountinfo`.
pub fn repeated_mounts_for(root: &Path, canonical: &Path) -> Vec<PathBuf> {
    let Ok(table) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    repeated_mounts(&parse_mountinfo(&table), canonical)
        .iter()
        .filter_map(|path| path.strip_prefix(canonical).ok())
        .map(|below| root.join(below))
        .collect()
}

/// The mount points in what macOS's `mount` prints.
///
/// One per line: `/dev/disk3s5 on /System/Volumes/Data (apfs, local, …)`.
/// The point is
/// between the first ` on ` and the last ` (`, so a name with spaces or
/// brackets in it survives.
pub fn parse_macos_mounts(output: &str) -> Vec<PathBuf> {
    output
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once(" on ")?;
            let (point, _) = rest.rsplit_once(" (")?;
            Some(PathBuf::from(point))
        })
        .collect()
}

/// Where the Data volume is mounted on macOS.
///
/// Since Catalina `/` is a read-only system volume, and everything the user
/// can write — `/Users`,
/// `/Applications`, `/Library`, `/private`, … — lives on the Data volume,
/// joined into `/` by firmlinks. The two report the same device, and
/// `/Users` and `/System/Volumes/Data/Users` are the same inode.
pub const MACOS_DATA_VOLUME: &str = "/System/Volumes/Data";

/// Directories under `root` a scan must never enter, whatever the volume
/// rules say, given where `root` really is (`canonical`).
///
/// On macOS: the Data volume's second mount, which is every firmlinked
/// directory again under another name, so walking it counts the disk twice;
/// and `/Network`, an automount point that would reach for a server. The
/// paths are returned in `root`'s own spelling, because that is how the walk
/// names what it finds.
pub fn never_scanned(root: &Path, canonical: &Path) -> Vec<PathBuf> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    [MACOS_DATA_VOLUME, "/Network"]
        .iter()
        .filter_map(|skip| Path::new(skip).strip_prefix(canonical).ok())
        .filter(|below| !below.as_os_str().is_empty())
        .map(|below| root.join(below))
        .collect()
}

/// The top of the disk `path` lives on: its mount point, from `statfs`.
///
/// The Data volume answers `/System/Volumes/Data`, but the disk a user
/// thinks of is `/`, which shows the same files under the names Finder uses
/// and adds the system volume beside them.
#[cfg(target_os = "macos")]
pub fn volume_root_for(path: &Path) -> Option<PathBuf> {
    let stat = rustix::fs::statfs(path).ok()?;
    let mount = PathBuf::from(c_chars(&stat.f_mntonname));
    if mount.as_os_str().is_empty() {
        return None;
    }
    Some(if mount == Path::new(MACOS_DATA_VOLUME) {
        PathBuf::from("/")
    } else {
        mount
    })
}

/// The device a path's filesystem is mounted from, such as
/// `/dev/disk3s5`, from `statfs`.
#[cfg(target_os = "macos")]
pub fn device_for(path: &Path) -> Option<String> {
    // `/` is a snapshot of the system volume (`disk3s1s1`); name the disk
    // the user's files are on instead, which is what the meter measures.
    let path = if path == Path::new("/") {
        Path::new(MACOS_DATA_VOLUME)
    } else {
        path
    };
    let stat = rustix::fs::statfs(path).ok()?;
    let device = c_chars(&stat.f_mntfromname);
    (!device.is_empty()).then_some(device)
}

/// A NUL-terminated C string field as text.
#[cfg(target_os = "macos")]
fn c_chars(field: &[std::ffi::c_char]) -> String {
    let bytes: Vec<u8> = field
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Nothing to leave out by path on Windows.
///
/// There, the only way into another volume below `root` is a folder that
/// volume is mounted on, which is a reparse point the walk treats as a link
/// and does not enter unless links are followed; other drives are separate
/// trees altogether.
#[cfg(windows)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the Option is the Unix answer when the mount table is missing"
)]
pub const fn foreign_mounts_for(_root: &Path) -> Option<Vec<PathBuf>> {
    Some(Vec::new())
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

    const MOUNTINFO: &str = "\
22 1 0:21 /@ / rw,relatime - btrfs /dev/mapper/root rw,subvol=/@
23 22 0:21 /@home /home rw,relatime - btrfs /dev/mapper/root rw,subvol=/@home
24 22 0:21 /@log /var/log rw,relatime - btrfs /dev/mapper/root rw,subvol=/@log
25 22 0:21 /@snapshots /.snapshots rw - btrfs /dev/mapper/root rw
26 22 0:30 / /data rw,relatime shared:5 - btrfs /dev/mapper/data rw
27 22 0:30 / /mnt/data rw,relatime shared:5 - btrfs /dev/mapper/data rw
28 22 0:31 / /tmp rw - tmpfs tmpfs rw
29 22 259:3 / /boot rw - vfat /dev/nvme0n1p1 rw
";

    fn repeated(table: &str, root: &str) -> Vec<PathBuf> {
        let mut paths =
            repeated_mounts(&parse_mountinfo(table), Path::new(root));
        paths.sort();
        paths
    }

    #[test]
    fn subvolumes_are_not_repeats_but_a_second_mount_of_a_disk_is() {
        assert_eq!(repeated(MOUNTINFO, "/"), [PathBuf::from("/mnt/data")]);
        assert!(repeated(MOUNTINFO, "/home/tobi").is_empty());
        assert!(repeated(MOUNTINFO, "/data").is_empty());
        assert!(
            repeated(MOUNTINFO, "/mnt/data").is_empty(),
            "the view that was asked for is scanned"
        );
    }

    #[test]
    fn a_bind_mount_inside_the_scan_is_left_out() {
        let table = format!(
            "{MOUNTINFO}30 23 0:21 /@home/tobi/src /srv/src rw - btrfs /dev/mapper/root rw\n"
        );
        assert_eq!(
            repeated(&table, "/"),
            [PathBuf::from("/mnt/data"), PathBuf::from("/srv/src")]
        );
        // From home, the original is outside the scan: nothing repeats.
        assert!(repeated(&table, "/home/tobi").is_empty());
        assert!(repeated(&table, "/srv").is_empty());
    }

    #[test]
    fn the_top_of_a_btrfs_disk_beside_its_subvolumes_is_counted_once() {
        let table = format!(
            "{MOUNTINFO}31 22 0:21 / /mnt/top rw - btrfs /dev/mapper/root rw\n"
        );
        // The root cannot be left out, so its copy under /mnt/top is; the
        // other subvolumes are seen under /mnt/top instead of twice.
        assert_eq!(
            repeated(&table, "/"),
            [
                PathBuf::from("/.snapshots"),
                PathBuf::from("/home"),
                PathBuf::from("/mnt/data"),
                PathBuf::from("/mnt/top/@"),
                PathBuf::from("/var/log"),
            ]
        );
    }

    #[test]
    fn mountinfo_escapes_are_decoded_once() {
        let mounts = parse_mountinfo(
            "not a mount line\n\
             40 22 8:1 /a\\040b /media/My\\040Disk\\134040 rw - ext4 /dev/sda1 rw\n",
        );
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].fs_root, Path::new("/a b"));
        assert_eq!(mounts[0].point, Path::new("/media/My Disk\\040"));
        assert_eq!(mounts[0].source, "/dev/sda1");
        assert_eq!(mounts[0].fstype, "ext4");
    }

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
    fn macos_mount_points_keep_spaces_and_brackets() {
        let output = "\
/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)
devfs on /dev (devfs, local, nobrowse)
/dev/disk3s5 on /System/Volumes/Data (apfs, local, journaled, nobrowse)
map auto_home on /System/Volumes/Data/home (autofs, automounted, nobrowse)
/dev/disk5s1 on /Volumes/My Disk (2) (apfs, local, nodev, nosuid)
//tobi@nas/share on /Volumes/share on nas (smbfs, nodev, nosuid)
";
        let points: Vec<PathBuf> = [
            "/",
            "/dev",
            MACOS_DATA_VOLUME,
            "/System/Volumes/Data/home",
            "/Volumes/My Disk (2)",
            "/Volumes/share on nas",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(parse_macos_mounts(output), points);
    }

    #[test]
    fn only_macos_skips_the_data_volume_and_in_the_roots_spelling() {
        let skipped = never_scanned(Path::new("/"), Path::new("/"));
        let below_system =
            never_scanned(Path::new("/System/"), Path::new("/System"));
        let inside =
            never_scanned(Path::new("/Users/tobi"), Path::new("/Users/tobi"));
        let itself = never_scanned(
            Path::new(MACOS_DATA_VOLUME),
            Path::new(MACOS_DATA_VOLUME),
        );
        if cfg!(target_os = "macos") {
            assert_eq!(
                skipped,
                vec![PathBuf::from(MACOS_DATA_VOLUME), "/Network".into()]
            );
            assert_eq!(
                below_system,
                vec![PathBuf::from("/System/Volumes/Data")]
            );
        } else {
            assert!(skipped.is_empty() && below_system.is_empty());
        }
        assert!(inside.is_empty());
        assert!(itself.is_empty(), "asking for it by name scans it");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn the_home_disk_on_macos_is_the_root_and_has_a_device() {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let home = home.expect("HOME is set");
        assert_eq!(volume_root_for(&home), Some(PathBuf::from("/")));
        let device = device_for(Path::new("/")).expect("a device");
        assert!(device.starts_with("/dev/disk"), "{device}");
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
}

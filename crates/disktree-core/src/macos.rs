//! What macOS does differently, kept in one place.
//!
//! Three things have no Linux-shaped answer here:
//!
//! * **The mount table.** There is no `/proc/self/mounts`; `getfsstat(2)` is
//!   what `mount` and `df` read. Foundation's volume list is not a
//!   substitute: it leaves out `/System/Volumes/Data` and `/dev`, the two
//!   mounts a scan of `/` most needs to know about.
//! * **Firmlinks.** `/Users` is the Data volume seen through the sealed
//!   system volume at `/`, so "the mount with the longest prefix" names the
//!   wrong volume for almost every path. The kernel knows the real one, and
//!   `statfs(2)` asks it.
//! * **The Trash.** The Finder's Trash is not an XDG directory. Moving there
//!   through `NSFileManager` is what keeps Put Back working.

use std::ffi::{OsString, c_char};
use std::io;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};

use objc2::rc::autoreleasepool;
use objc2_foundation::{NSArray, NSFileManager, NSNumber, NSString, NSURL};

use crate::space::Mount;

/// Where the Data volume is mounted. Everything a user owns lives on it,
/// and firmlinks show it again at `/`.
pub const DATA_VOLUME: &str = "/System/Volumes/Data";

/// Every mounted filesystem, as `getfsstat(2)` reports it.
///
/// `MNT_NOWAIT` returns what the kernel already has instead of asking each
/// filesystem, so a network share that has gone away cannot stall the scan.
#[allow(
    unsafe_code,
    reason = "getfsstat(2) has no safe wrapper in std or rustix. The buffer \
              is sized from the kernel's own count, and only the entries it \
              reports as filled are ever read."
)]
pub fn mount_table() -> Option<Vec<Mount>> {
    // SAFETY: a null buffer with size 0 only asks for the number of mounts.
    let count =
        unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    // Headroom for mounts that appear between the two calls; the kernel
    // truncates to the buffer rather than overrunning it.
    let capacity = usize::try_from(count).ok()? + 8;
    let mut entries: Vec<libc::statfs> = Vec::with_capacity(capacity);
    let bytes = i32::try_from(capacity * size_of::<libc::statfs>()).ok()?;
    // SAFETY: `entries` owns room for `bytes` bytes, and the kernel writes
    // at most that many.
    let filled = unsafe {
        libc::getfsstat(entries.as_mut_ptr(), bytes, libc::MNT_NOWAIT)
    };
    let filled = usize::try_from(filled).ok()?.min(capacity);
    // SAFETY: the kernel initialised the first `filled` entries.
    unsafe { entries.set_len(filled) };
    Some(entries.iter().map(mount_from).collect())
}

fn mount_from(entry: &libc::statfs) -> Mount {
    Mount {
        source: lossy(&entry.f_mntfromname),
        point: path_from(&entry.f_mntonname),
        fstype: lossy(&entry.f_fstypename),
        // Nothing on macOS reads mount options: APFS snapshots are mounted
        // from their own source, so the source test already leaves them out.
        options: String::new(),
    }
}

/// The mount point of the volume `path` really lives on, firmlinks
/// resolved: `/System/Volumes/Data` for anything under `/Users`.
pub fn mount_point_of(path: &Path) -> Option<PathBuf> {
    let stat = rustix::fs::statfs(path).ok()?;
    Some(path_from(&stat.f_mntonname))
}

/// The top of the disk `path` lives on.
///
/// For the Data volume that is `/`, not its own mount point: `/` shows the
/// sealed system and, through firmlinks, every user file once, so a scan
/// there is the whole disk with nothing counted twice. Scanning
/// `/System/Volumes/Data` would show the same files under a path nobody
/// recognises and leave the system out.
pub fn volume_root_of(path: &Path) -> Option<PathBuf> {
    let point = mount_point_of(path)?;
    Some(if point == Path::new(DATA_VOLUME) {
        PathBuf::from("/")
    } else {
        point
    })
}

/// Bytes the system would free up for a new file on `path`'s volume.
///
/// This is the number the Finder and System Settings show. It is larger
/// than `statvfs`'s free count because it includes purgeable space —
/// caches, local Time Machine snapshots and iCloud files the system is
/// ready to drop on demand. `None` where Foundation cannot say.
pub fn available_for_important_use(path: &Path) -> Option<u64> {
    autoreleasepool(|_| {
        let url = file_url(path).ok()?;
        // Foundation's resource keys are strings equal to their own names;
        // building one by name avoids reading an extern static, which
        // would need `unsafe`.
        let key = NSString::from_str(
            "NSURLVolumeAvailableCapacityForImportantUsageKey",
        );
        let values = url
            .resourceValuesForKeys_error(&NSArray::from_slice(&[&*key]))
            .ok()?;
        let number = values.objectForKey(&key)?.downcast::<NSNumber>().ok()?;
        u64::try_from(number.longLongValue()).ok()
    })
}

/// Move `path` to the Trash of its volume, as the Finder would, so Put Back
/// can restore it. Returns where it landed, when the system says.
pub fn move_to_trash(path: &Path) -> io::Result<Option<PathBuf>> {
    autoreleasepool(|_| {
        let url = file_url(path)?;
        let mut landed = None;
        NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, Some(&mut landed))
            .map_err(|error| {
                io::Error::other(error.localizedDescription().to_string())
            })?;
        Ok(landed
            .and_then(|url| url.path())
            .map(|path| PathBuf::from(path.to_string())))
    })
}

fn file_url(path: &Path) -> io::Result<objc2::rc::Retained<NSURL>> {
    // APFS and HFS+ names are UTF-8 by construction; anything else did not
    // come from a macOS filesystem.
    let text = path.to_str().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path is not UTF-8")
    })?;
    Ok(NSURL::fileURLWithPath(&NSString::from_str(text)))
}

/// The bytes of a fixed-size, NUL-terminated C string field.
fn bytes_of(field: &[c_char]) -> Vec<u8> {
    field
        .iter()
        .take_while(|&&byte| byte != 0)
        // `c_char` is `i8` on Apple targets; this reinterprets, it does not
        // convert.
        .map(|&byte| byte as u8)
        .collect()
}

fn lossy(field: &[c_char]) -> String {
    String::from_utf8_lossy(&bytes_of(field)).into_owned()
}

fn path_from(field: &[c_char]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes_of(field)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_has_the_system_and_data_volumes() {
        let mounts = mount_table().expect("getfsstat works");
        let root = mounts
            .iter()
            .find(|mount| mount.point == Path::new("/"))
            .expect("/ is mounted");
        let data = mounts
            .iter()
            .find(|mount| mount.point == Path::new(DATA_VOLUME))
            .expect("the Data volume is mounted");
        assert_eq!(root.fstype, "apfs");
        assert_ne!(root.source, data.source, "separate volumes");
        assert!(mounts.iter().any(|mount| mount.point == Path::new("/dev")));
    }

    #[test]
    fn a_whole_disk_scan_of_root_leaves_the_data_volume_out() {
        // Firmlinks already show the Data volume under `/Users` and friends;
        // walking its mount point as well would count every file twice.
        let mounts = mount_table().expect("getfsstat works");
        let foreign = crate::space::foreign_mounts(&mounts, Path::new("/"));
        assert!(foreign.contains(&PathBuf::from(DATA_VOLUME)), "{foreign:?}");
        assert!(foreign.contains(&PathBuf::from("/dev")), "{foreign:?}");
    }

    #[test]
    fn firmlinks_resolve_to_the_data_volume() {
        let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
        assert_eq!(
            mount_point_of(&home).as_deref(),
            Some(Path::new(DATA_VOLUME))
        );
        assert_eq!(volume_root_of(&home).as_deref(), Some(Path::new("/")));
    }

    #[test]
    fn important_capacity_includes_at_least_the_plain_free_space() {
        let temp = std::env::temp_dir();
        let important =
            available_for_important_use(&temp).expect("Foundation answers");
        let plain = rustix::fs::statvfs(&temp).expect("statvfs");
        let plain = plain.f_bavail * plain.f_frsize;
        // Purgeable space only ever adds; allow for writes by other
        // processes between the two reads.
        assert!(important + (1 << 30) >= plain, "{important} < {plain}");
    }

    #[test]
    fn the_trash_accepts_a_file_and_it_leaves_its_folder() {
        let temp = tempfile::tempdir().expect("temp dir");
        let doomed = temp.path().join("disktree-trash-test.txt");
        std::fs::write(&doomed, "bye").expect("write");
        let landed = move_to_trash(&doomed)
            .expect("moved to the Trash")
            .expect("the system says where");
        assert!(!doomed.exists());
        assert!(landed.exists(), "{}", landed.display());
        assert!(
            landed.components().any(|part| part.as_os_str() == ".Trash"),
            "in the Finder's Trash, not an XDG one: {}",
            landed.display()
        );
        // Leave the user's Trash as it was.
        std::fs::remove_file(&landed).expect("clean up");
    }

    #[test]
    fn a_missing_path_is_an_error_not_a_panic() {
        let error = move_to_trash(Path::new("/definitely/not/here"))
            .expect_err("nothing to trash");
        assert!(!error.to_string().is_empty());
    }
}

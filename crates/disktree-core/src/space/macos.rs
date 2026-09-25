use std::path::{Path, PathBuf};

use libc::{MNT_NOWAIT, c_char, statfs};

pub fn volume_root(path: &Path) -> Option<PathBuf> {
    let stat = rustix::fs::statfs(path).ok()?;
    fixed_os(&stat.f_mntonname).map(PathBuf::from)
}

pub fn device(path: &Path) -> Option<String> {
    let stat = rustix::fs::statfs(path).ok()?;
    fixed_os(&stat.f_mntfromname)
        .map(|value| value.to_string_lossy().into_owned())
}

pub fn foreign_mounts(root: &Path) -> Option<Vec<PathBuf>> {
    use rustix::fs::{Mode, OFlags};
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    let fd = rustix::fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY,
        Mode::empty(),
    )
    .ok()?;
    let fs_root = rustix::fs::getpath(&fd).ok()?;
    let fs_root = PathBuf::from(OsStr::from_bytes(fs_root.to_bytes()));
    let mounts = all_mounts()?;
    let mount_points: Vec<PathBuf> = mounts
        .into_iter()
        .filter_map(|mount| fixed_os(&mount.f_mntonname).map(PathBuf::from))
        .collect();
    Some(excluded_mounts(&mount_points, &fs_root, root))
}

fn excluded_mounts(
    mount_points: &[PathBuf],
    fs_root: &Path,
    root: &Path,
) -> Vec<PathBuf> {
    let mut excluded = Vec::new();
    for mount_point in mount_points {
        let aliases = mount_aliases(mount_point);
        if mount_point == fs_root
            || (fs_root != Path::new("/")
                && aliases.iter().any(|point| point == fs_root))
        {
            continue;
        }
        for point in aliases {
            if point != fs_root
                && let Ok(suffix) = point.strip_prefix(fs_root)
            {
                excluded.push(root.join(suffix));
            }
        }
    }
    excluded
}

fn mount_aliases(point: &Path) -> Vec<PathBuf> {
    let mut aliases = vec![point.to_path_buf()];
    let data = Path::new("/System/Volumes/Data");
    if let Ok(suffix) = point.strip_prefix(data) {
        aliases.push(Path::new("/").join(suffix));
    } else if point.is_absolute() {
        aliases.push(data.join(point.strip_prefix("/").unwrap_or(point)));
    }
    aliases
}

#[allow(unsafe_code, reason = "getfsstat requires an initialized C buffer")]
fn all_mounts() -> Option<Vec<statfs>> {
    // The first call asks the kernel for the current mount count without
    // touching any automount point. One spare record detects table growth.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, MNT_NOWAIT) };
    if count < 0 {
        return None;
    }
    let capacity = usize::try_from(count).ok()?.checked_add(1)?;
    let mut mounts = vec![statfs_zeroed(); capacity];
    let bytes = capacity
        .checked_mul(std::mem::size_of::<statfs>())
        .and_then(|size| libc::c_int::try_from(size).ok())?;
    let actual =
        unsafe { libc::getfsstat(mounts.as_mut_ptr(), bytes, MNT_NOWAIT) };
    if actual < 0 || usize::try_from(actual).ok()? >= capacity {
        return None;
    }
    mounts.truncate(usize::try_from(actual).ok()?);
    Some(mounts)
}

#[allow(
    unsafe_code,
    reason = "the C API requires an initialized statfs record"
)]
const fn statfs_zeroed() -> statfs {
    unsafe { std::mem::zeroed() }
}

fn fixed_os(bytes: &[c_char]) -> Option<std::ffi::OsString> {
    use std::os::unix::ffi::OsStringExt as _;
    let end = bytes.iter().position(|byte| *byte == 0)?;
    Some(std::ffi::OsString::from_vec(
        bytes[..end].iter().map(|byte| *byte as u8).collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_mount_aliases_are_component_safe_and_bidirectional() {
        assert!(
            mount_aliases(Path::new("/System/Volumes/Data/Volumes/external"))
                .contains(&PathBuf::from("/Volumes/external"))
        );
        assert!(
            mount_aliases(Path::new("/Volumes/external")).contains(
                &PathBuf::from("/System/Volumes/Data/Volumes/external")
            )
        );
        assert!(
            !mount_aliases(Path::new("/System/Volumes/Database"))
                .contains(&PathBuf::from("/base"))
        );
    }

    #[test]
    fn mounted_children_are_excluded_from_the_filesystem_spelling() {
        let fs_root = Path::new("/System/Volumes/Data/Users/Åsa");
        let requested = Path::new("/Users/åsa");
        let mounts = [
            PathBuf::from("/System/Volumes/Data/Users/Åsa/Volumes/remote"),
            PathBuf::from("/Volumes/remote"),
            PathBuf::from("/home/autofs"),
            PathBuf::from("/Users/Åsa/NAS"),
            PathBuf::from("/private/mnt/remote"),
        ];
        let excluded = excluded_mounts(&mounts, fs_root, requested);
        assert_eq!(excluded.len(), 2);
        assert!(excluded.contains(&requested.join("Volumes/remote")));
        assert!(excluded.contains(&requested.join("NAS")));
        let all = excluded_mounts(
            &mounts,
            Path::new("/System/Volumes/Data"),
            Path::new("/"),
        );
        assert_eq!(all.len(), 5);
        assert!(all.contains(&PathBuf::from("/home/autofs")));
        assert!(all.contains(&PathBuf::from("/private/mnt/remote")));
    }

    #[test]
    fn fixed_strings_preserve_non_utf8_native_bytes() {
        let mut bytes = [c_char::default(); 8];
        bytes[0] = i8::from_ne_bytes(*b"/");
        bytes[1] = i8::from_ne_bytes([0x80]);
        let value = fixed_os(&bytes).expect("NUL-terminated native string");
        assert_eq!(value.as_encoded_bytes(), b"/\x80");
    }

    #[test]
    fn root_scan_excludes_both_spellings_of_an_automount() {
        let mounts = [
            PathBuf::from("/"),
            PathBuf::from("/System/Volumes/Data"),
            PathBuf::from("/System/Volumes/Data/home"),
        ];
        let excluded = excluded_mounts(&mounts, Path::new("/"), Path::new("/"));
        assert!(excluded.contains(&PathBuf::from("/home")));
        assert!(excluded.contains(&PathBuf::from("/System/Volumes/Data/home")));
        assert!(!excluded.contains(&PathBuf::from("/")));
        let data = Path::new("/System/Volumes/Data");
        let excluded = excluded_mounts(&mounts, data, data);
        assert_eq!(excluded, vec![data.join("home")]);
    }
}

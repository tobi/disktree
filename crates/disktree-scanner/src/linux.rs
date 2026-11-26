//! Zero-allocation Linux directory walker built on `openat` + 64 KiB
//! `getdents64` + relative `statx(dirfd, d_name, AT_SYMLINK_NOFOLLOW |
//! AT_STATX_DONT_SYNC)`.
//!
//! Avoids constructing a `PathBuf`, `OsString`, or `CString` for leaf entries
//! and avoids full-path kernel VFS dentry traversal from `/` on every file.

use std::cell::RefCell;
use std::ffi::CStr;
use std::io;
use std::mem::MaybeUninit;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use rustix::fd::AsFd;
use rustix::fs::{
    AtFlags, CWD, FileType, Mode, OFlags, RawDir, StatxFlags, makedev, openat,
    statat, statx,
};

use crate::types::NodeKind;

/// 64 KiB `getdents64` read buffer reused per worker thread (zero per-dir
/// allocation; 32x larger than musl/glibc's default `readdir` buffer).
const GETDENTS_BUF_BYTES: usize = 64 * 1024;

const STATX_MASK: StatxFlags = StatxFlags::TYPE
    .union(StatxFlags::SIZE)
    .union(StatxFlags::BLOCKS)
    .union(StatxFlags::MTIME)
    .union(StatxFlags::INO)
    .union(StatxFlags::NLINK);

/// Flipped to `false` on the first `ENOSYS` so older kernels or strict seccomp
/// filters fall back to `statat(dirfd, ...)` cleanly without repeating errors.
static STATX_SUPPORTED: AtomicBool = AtomicBool::new(true);

thread_local! {
    static GETDENTS_BUF: RefCell<Box<[MaybeUninit<u8>]>> = RefCell::new(
        vec![MaybeUninit::uninit(); GETDENTS_BUF_BYTES].into_boxed_slice(),
    );
}

#[derive(Clone, Copy, Debug)]
pub struct EntryStat {
    pub kind: NodeKind,
    pub device: u64,
    pub allocated_bytes: u64,
    pub apparent_bytes: u64,
    pub modified: i64,
    pub hardlink: Option<(u64, u64)>,
    pub inode: Option<(u64, u64)>,
}

/// Read every entry of `dir_path` using a single `openat` + 64 KiB `getdents64`
/// chunks, passing each non-`.`/non-`..` entry's raw NUL-terminated `d_name`,
/// `d_type`, and open `dirfd` to `visit` with zero heap allocations.
pub fn scan_dir_entries<F>(dir_path: &Path, mut visit: F) -> io::Result<()>
where
    F: FnMut(rustix::fd::BorrowedFd<'_>, &CStr, FileType) -> io::Result<bool>,
{
    let dirfd = openat(
        CWD,
        dir_path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;

    GETDENTS_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        let mut raw_dir = RawDir::new(&dirfd, &mut buf[..]);
        while let Some(entry_res) = raw_dir.next() {
            let entry = entry_res.map_err(io::Error::from)?;
            let name = entry.file_name();
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let keep_going = visit(dirfd.as_fd(), name, entry.file_type())?;
            if !keep_going {
                break;
            }
        }
        Ok(())
    })
}

/// Stat `name` relative to `dirfd` with `statx(..., AT_STATX_DONT_SYNC)` (or
/// `statat` fallback on `ENOSYS`), performing zero path allocations.
pub fn stat_at(
    dirfd: rustix::fd::BorrowedFd<'_>,
    name: &CStr,
    follow_links: bool,
) -> io::Result<EntryStat> {
    if STATX_SUPPORTED.load(Ordering::Relaxed) {
        let flags = if follow_links {
            AtFlags::STATX_DONT_SYNC
        } else {
            AtFlags::SYMLINK_NOFOLLOW | AtFlags::STATX_DONT_SYNC
        };
        match statx(dirfd, name, flags, STATX_MASK) {
            Ok(st) => {
                let kind = kind_from_mode(u32::from(st.stx_mode));
                let device = makedev(st.stx_dev_major, st.stx_dev_minor);
                let allocated_bytes = st.stx_blocks.saturating_mul(512);
                let apparent_bytes = st.stx_size;
                let modified = st.stx_mtime.tv_sec;
                let inode = (st.stx_ino != 0).then_some((device, st.stx_ino));
                let hardlink = (st.stx_nlink > 1).then_some(inode).flatten();
                return Ok(EntryStat {
                    kind,
                    device,
                    allocated_bytes,
                    apparent_bytes,
                    modified,
                    hardlink,
                    inode,
                });
            }
            Err(rustix::io::Errno::NOSYS | rustix::io::Errno::PERM) => {
                STATX_SUPPORTED.store(false, Ordering::Relaxed);
            }
            Err(err) => return Err(io::Error::from(err)),
        }
    }

    let flags = if follow_links {
        AtFlags::empty()
    } else {
        AtFlags::SYMLINK_NOFOLLOW
    };
    let st = statat(dirfd, name, flags).map_err(io::Error::from)?;
    let kind = kind_from_mode(st.st_mode);
    let device = st.st_dev;
    let allocated_bytes =
        u64::try_from(st.st_blocks).unwrap_or(0).saturating_mul(512);
    let apparent_bytes = u64::try_from(st.st_size).unwrap_or(0);
    let modified = st.st_mtime;
    let inode = (st.st_ino != 0).then_some((device, st.st_ino));
    let hardlink = (st.st_nlink > 1).then_some(inode).flatten();
    Ok(EntryStat {
        kind,
        device,
        allocated_bytes,
        apparent_bytes,
        modified,
        hardlink,
        inode,
    })
}

const fn kind_from_mode(mode: u32) -> NodeKind {
    // POSIX S_IFMT = 0o170000
    match mode & 0o170_000 {
        0o040_000 => NodeKind::Directory,
        0o100_000 => NodeKind::File,
        0o120_000 => NodeKind::Symlink,
        _ => NodeKind::Other,
    }
}

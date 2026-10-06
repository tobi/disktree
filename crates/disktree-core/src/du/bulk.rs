//! macOS: a directory's entries and their stats in one system call.
//!
//! `readdir` then `fstatat` per entry is two trips into the kernel for every
//! file, and the per-file lookup is most of what a walk costs.
//! `getattrlistbulk` returns a buffer of entries, each with the attributes
//! asked for, for about what the listing costs alone.
//!
//! Only what can be turned into exactly the `struct stat` du would have seen
//! is taken from it: regular files and symbolic links with a single link.
//! XNU's `vn_stat` fills `st_blocks` as the total allocation rounded up to
//! 512-byte blocks and `st_size` as the data fork's length, which are
//! `ATTR_FILE_ALLOCSIZE` and `ATTR_FILE_DATALENGTH` here. Directories,
//! multiply linked files (whose id here can be the link's rather than the
//! inode's), anything else, and any entry the call reports an error for are
//! left for `fstatat`.

#![allow(
    unsafe_code,
    reason = "getattrlistbulk, which std does not wrap; the call says why \
              it is sound, and the buffer is then read with safe code"
)]

use std::cell::RefCell;
use std::os::fd::AsRawFd as _;

use rustix::fd::BorrowedFd;
use rustix::io::Errno;

use super::walk::{Meta, Time};

/// `ATTR_CMN_ERROR`, which the libc crate does not declare.
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
/// `vtype` values of `ATTR_CMN_OBJTYPE`.
const VREG: u32 = 1;
const VLNK: u32 = 5;

const COMMON: u32 = libc::ATTR_CMN_RETURNED_ATTRS
    | libc::ATTR_CMN_NAME
    | libc::ATTR_CMN_DEVID
    | libc::ATTR_CMN_OBJTYPE
    | libc::ATTR_CMN_MODTIME
    | libc::ATTR_CMN_CHGTIME
    | libc::ATTR_CMN_ACCTIME
    | libc::ATTR_CMN_ACCESSMASK
    | libc::ATTR_CMN_FILEID
    | ATTR_CMN_ERROR;
const FILE: u32 = libc::ATTR_FILE_LINKCOUNT
    | libc::ATTR_FILE_ALLOCSIZE
    | libc::ATTR_FILE_DATALENGTH;

/// Large enough for a few hundred entries a call; each thread keeps one.
const BUFFER: usize = 64 * 1024;

thread_local! {
    static BUF: RefCell<Vec<u8>> = RefCell::new(vec![0; BUFFER]);
}

/// One entry: its name, its id for `fts`'s inode sort, and its stat when
/// the bulk attributes give it exactly.
#[derive(Debug)]
pub struct Item {
    pub name: Box<[u8]>,
    pub id: u64,
    pub meta: Option<Meta>,
}

/// Whether to use this at all; `DISKTREE_DU_BULK=0` turns it off, to
/// compare against the plain walk.
pub fn enabled() -> bool {
    std::env::var_os("DISKTREE_DU_BULK").is_none_or(|v| v != "0")
}

/// Every entry of the directory open as `dir`, in the order the file
/// system returns them. On failure part way, what was read and the error.
/// `None` when a record does not parse: the caller then lists the
/// directory the ordinary way rather than risk losing an entry.
pub fn read(dir: BorrowedFd<'_>) -> Option<(Vec<Item>, Option<Errno>)> {
    BUF.with_borrow_mut(|buf| {
        let mut items = Vec::new();
        let mut list = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: COMMON,
            volattr: 0,
            dirattr: 0,
            fileattr: FILE,
            forkattr: 0,
        };
        loop {
            // SAFETY: `list` is a valid attrlist, `buf` is a live, writable
            // buffer of the length passed, and `dir` is an open directory
            // for as long as the call runs. The kernel writes at most
            // `buf.len()` bytes and returns how many entries it wrote.
            let count = unsafe {
                libc::getattrlistbulk(
                    dir.as_raw_fd(),
                    (&raw mut list).cast(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    u64::from(libc::FSOPT_PACK_INVAL_ATTRS),
                )
            };
            if count == 0 {
                return Some((items, None));
            }
            let Ok(count) = usize::try_from(count) else {
                // Failing before the first entry, as on a directory that
                // can be read but not searched, or a file system without
                // the call, leaves the listing to `readdir`, which reports
                // what GNU does.
                if items.is_empty() {
                    return None;
                }
                let error = std::io::Error::last_os_error();
                let errno = error.raw_os_error().unwrap_or(libc::EIO);
                return Some((items, Some(Errno::from_raw_os_error(errno))));
            };
            let mut at = 0;
            for _ in 0..count {
                let len = u32_at(buf, at)? as usize;
                let record = buf.get(at..at.checked_add(len)?)?;
                items.push(parse(record)?);
                at += len;
            }
        }
    })
}

fn u32_at(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

fn i32_at(buf: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_ne_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(buf: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(buf.get(at..at + 8)?.try_into().ok()?))
}

fn i64_at(buf: &[u8], at: usize) -> Option<i64> {
    Some(i64::from_ne_bytes(buf.get(at..at + 8)?.try_into().ok()?))
}

fn time_at(buf: &[u8], at: usize) -> Option<Time> {
    let nsec = i64_at(buf, at + 8)?;
    Some(Time {
        sec: i64_at(buf, at)?,
        nsec: u32::try_from(nsec.clamp(0, 999_999_999)).ok()?,
    })
}

/// One record of the buffer. With `FSOPT_PACK_INVAL_ATTRS` every asked-for
/// attribute has its slot, so the offsets are fixed; which ones hold a value
/// is in the leading `attribute_set_t`. The slots follow bit order, except
/// that `ATTR_CMN_ERROR` comes first, as getattrlistbulk(2)'s example reads
/// it. Its bit is always returned; its value is the error.
fn parse(record: &[u8]) -> Option<Item> {
    // length(4), returned attribute_set_t(20), error(4), then the rest.
    let returned_common = u32_at(record, 4)?;
    let returned_file = u32_at(record, 4 + 12)?;
    let error = u32_at(record, 24)?;
    let mut at = 28;
    let name_ref = at;
    let name_offset = usize::try_from(i32_at(record, at)?).ok()?;
    let name_len = u32_at(record, at + 4)? as usize;
    at += 8;
    let dev = i32_at(record, at)?;
    at += 4;
    let objtype = u32_at(record, at)?;
    at += 4;
    let mtime = time_at(record, at)?;
    at += 16;
    let ctime = time_at(record, at)?;
    at += 16;
    let atime = time_at(record, at)?;
    at += 16;
    let access = u32_at(record, at)?;
    at += 4;
    let id = u64_at(record, at)?;
    at += 8;
    // A directory's record stops here: it has no file attributes to pack.
    let file = || {
        Some((
            u32_at(record, at)?,
            i64_at(record, at + 4)?,
            i64_at(record, at + 12)?,
        ))
    };
    let file = if returned_file & FILE == FILE {
        file()
    } else {
        None
    };

    // The name's length counts its terminating NUL.
    let start = name_ref + name_offset;
    let name = record.get(start..start + name_len.checked_sub(1)?)?;

    let required = COMMON & !(ATTR_CMN_ERROR | libc::ATTR_CMN_RETURNED_ATTRS);
    let usable = error == 0
        && returned_common & required == required
        && (objtype == VREG || objtype == VLNK);
    let meta = file.filter(|&(links, ..)| usable && links == 1).map(
        |(_, alloc, length)| {
            let kind = if objtype == VREG {
                libc::S_IFREG
            } else {
                libc::S_IFLNK
            };
            Meta {
                // Widened as `st_dev as u64` is in `Meta::from_stat`.
                dev: i64::from(dev).cast_unsigned(),
                ino: id,
                mode: u32::from(kind) | (access & 0o7777),
                nlink: 1,
                // `vn_stat`: roundup(va_total_alloc, 512) / 512 blocks.
                blocks: u64::try_from(alloc).unwrap_or(0).div_ceil(512),
                size: length,
                mtime,
                atime,
                ctime,
            }
        },
    );
    Some(Item {
        name: name.into(),
        id,
        meta,
    })
}

//! What a walk on macOS needs beyond the standard library.
//!
//! `std::fs::read_dir` lists a directory with `readdir`, whose records carry
//! a name and a type, and `DirEntry::metadata` is then an `lstat` of each
//! entry's full path. `getattrlistbulk(2)` returns every entry's size,
//! identity, age and flags for a whole buffer of entries, without the kernel
//! setting up per-file state for each one as a stat must.

#![allow(
    unsafe_code,
    reason = "Darwin attribute calls, which neither std nor rustix wraps; each block \
              says why it is sound"
)]

use std::cell::RefCell;
use std::ffi::{CString, OsStr, c_int, c_void};
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt as _;

use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};

use crate::sharing::CloneInfo;

/// Bytes the kernel fills per call: some hundreds of entries.
const BUFFER_BYTES: usize = 64 * 1024;

const REFCNT: u32 = 0x1000;
const FULL_SHARE: u64 = 0x40;
const MAY_SHARE: u64 = 0x1 | FULL_SHARE;
const EXTENDED: u32 = libc::FSOPT_ATTR_CMN_EXTENDED;
const RETURNED: u32 = libc::ATTR_CMN_RETURNED_ATTRS;
const REAL_DEVICE: u32 = 0x40;
/// A file's clone group: cheap to list, since APFS keeps it in the inode.
const IDS: u32 = REAL_DEVICE | libc::ATTR_CMNEXT_CLONEID;
/// The group and whether it shares blocks. The flags are dear to list:
/// for every file that was ever cloned APFS looks up how many files share
/// its data, and listing threads queue for that lookup in the kernel. On
/// 8.9 million files of agent worktrees, 7.8 million of them clones, the
/// flags made an 18-thread walk 16 s slower (27 s without them, 43 s with;
/// the group alone cost about 1 s), and the more threads, the more each
/// lookup cost: 3 us of system time a clone on 4 threads, 20 us on 16.
const HINTS: u32 = IDS | libc::ATTR_CMNEXT_EXT_FLAGS;

// From <sys/attr.h> and <sys/vnode.h>, which the libc crate does not cover.
const ATTR_BIT_MAP_COUNT: u16 = 5;
const ATTR_CMN_NAME: u32 = 0x0000_0001;
const ATTR_CMN_DEVID: u32 = 0x0000_0002;
const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
const ATTR_CMN_FLAGS: u32 = 0x0004_0000;
const ATTR_CMN_FILEID: u32 = 0x0200_0000;
const ATTR_CMN_ERROR: u32 = 0x2000_0000;
const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;
const ATTR_FILE_LINKCOUNT: u32 = 0x0000_0001;
/// Every fork, which is what `st_blocks` counts: a resource fork is on the
/// disk too.
const ATTR_FILE_ALLOCSIZE: u32 = 0x0000_0004;
/// The data fork alone, which is what `st_size` is.
const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;
const VREG: u32 = 1;
const VDIR: u32 = 2;
const VLNK: u32 = 5;
/// `SF_DATALESS` from <sys/stat.h>.
const SF_DATALESS: u32 = 0x4000_0000;

const COMMON_ATTRIBUTES: u32 = ATTR_CMN_RETURNED_ATTRS
    | ATTR_CMN_NAME
    | ATTR_CMN_DEVID
    | ATTR_CMN_OBJTYPE
    | ATTR_CMN_MODTIME
    | ATTR_CMN_FLAGS
    | ATTR_CMN_FILEID
    | ATTR_CMN_ERROR;
const FILE_ATTRIBUTES: u32 =
    ATTR_FILE_LINKCOUNT | ATTR_FILE_ALLOCSIZE | ATTR_FILE_DATALENGTH;
/// What [`Cursor::entry`] reads after the name; a file system may leave
/// any of them out.
const MEASURED: u32 = ATTR_CMN_DEVID
    | ATTR_CMN_OBJTYPE
    | ATTR_CMN_MODTIME
    | ATTR_CMN_FLAGS
    | ATTR_CMN_FILEID;

#[repr(C)]
struct AttrList {
    bitmapcount: u16,
    reserved: u16,
    commonattr: u32,
    volattr: u32,
    dirattr: u32,
    fileattr: u32,
    forkattr: u32,
}

unsafe extern "C" {
    fn getattrlistbulk(
        dirfd: c_int,
        attr_list: *mut AttrList,
        attr_buf: *mut c_void,
        attr_buf_size: usize,
        options: u64,
    ) -> c_int;
}

thread_local! {
    /// The last finished listing's buffer, for the next one on this thread:
    /// a walk lists millions of directories, one at a time per thread.
    static SPARE: RefCell<Option<Box<[u64]>>> = const { RefCell::new(None) };
}

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Directory,
    Link,
    File,
    /// A FIFO, a socket or a device node.
    Other,
}

/// One directory entry, with what a measurement needs.
#[derive(Debug)]
pub struct Entry {
    name: Box<str>,
    /// The name as the file system spells it, only when `name` could not:
    /// a name that is not UTF-8 is shown lossy but must still open.
    exact: Option<Box<OsStr>>,
    kind: Kind,
    apparent: u64,
    allocated: u64,
    modified: i64,
    device: u64,
    inode: u64,
    links: u32,
    flags: u32,
    hints: Option<Hints>,
}

/// What a listing says about an entry's clone group.
#[derive(Clone, Copy, Debug)]
struct Hints {
    /// `ATTR_CMNEXT_REALDEVID`: zero when the file system did not say.
    device: u64,
    id: u64,
    /// `ATTR_CMNEXT_EXT_FLAGS`, when the listing asked for them.
    flags: Option<u64>,
}

/// What a listing asks about clones; see [`IDS`] and [`HINTS`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clones {
    Off,
    /// Clone groups only. A file's sharing flags then come from another
    /// member of its group met earlier in the scan, or from the file's own
    /// metadata read, which most clones pay for anyway.
    Ids,
    /// Clone groups and every file's sharing flags.
    Flags,
}

impl Clones {
    const fn attributes(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Ids => IDS,
            Self::Flags => HINTS,
        }
    }
}

impl Entry {
    /// The entry's path inside `dir`, the directory it was listed from.
    pub fn path(&self, dir: &Path) -> PathBuf {
        dir.join(self.file_name())
    }

    pub fn file_name(&self) -> &OsStr {
        self.exact
            .as_deref()
            .unwrap_or_else(|| OsStr::new(&*self.name))
    }

    /// The name as text; see [`Self::file_name`] for what it may lose.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The name, moved out: it is copied once, into the tree's names. The
    /// entry's other accessors keep working; `name`, `file_name` and
    /// `path` do not.
    pub fn take_name(&mut self) -> Box<str> {
        std::mem::take(&mut self.name)
    }

    pub const fn kind(&self) -> Kind {
        self.kind
    }

    /// `st_size`.
    pub const fn apparent(&self) -> u64 {
        self.apparent
    }

    /// `st_blocks * 512`.
    pub const fn allocated(&self) -> u64 {
        self.allocated
    }

    /// Last write, in Unix seconds.
    pub const fn modified(&self) -> i64 {
        self.modified
    }

    /// `(st_dev, st_ino)`: the same for two names of one file.
    pub fn identity(&self) -> (u64, u64) {
        (
            self.hints.map_or(self.device, |hints| {
                if hints.device == 0 {
                    self.device
                } else {
                    hints.device
                }
            }),
            self.inode,
        )
    }

    pub const fn device(&self) -> u64 {
        self.device
    }

    /// Whether the file has more than one name.
    pub const fn links(&self) -> u64 {
        self.links as u64
    }

    pub const fn shared(&self) -> bool {
        self.links > 1
    }

    /// Whether this is a directory iCloud Drive or a File Provider has
    /// evicted, which listing would make it fetch.
    pub const fn evicted(&self) -> bool {
        matches!(self.kind, Kind::Directory) && self.flags & SF_DATALESS != 0
    }

    /// Only clone candidates pay for private-size reads; ordinary metadata
    /// and identity remain the values returned by the bulk listing.
    ///
    /// A listing without sharing flags takes them from the file's clone
    /// group. A clone ID names a data stream, and files with the same one
    /// are pure clones of each other (getattrlist(2)); APFS keeps the
    /// stream's reference count with the stream, not with each file (the
    /// Apple File System Reference, `j_dstream_id_val_t`). So a file whose
    /// group shares all its blocks does too: across 2.9 million groups on
    /// three trees (worktrees, a home Library and a folder of checkouts)
    /// no two files of a group disagreed. A group the scan has not met yet
    /// is read from the file itself, in the read that measures its private
    /// size.
    pub fn clone_info(
        &self,
        dir: &Path,
        cache: &crate::clone_cache::Cache,
    ) -> Option<CloneInfo> {
        let hints = self.hints?;
        if self.kind != Kind::File {
            return None;
        }
        // The path is joined only for a read: most clones never need one.
        resolve(
            hints,
            self.inode,
            u64::from(self.links),
            self.allocated,
            cache,
            || {
                private_info(
                    &self.path(dir),
                    hints.device,
                    self.inode,
                    hints.id,
                )
            },
        )
    }

    fn named(name: &[u8]) -> (Box<str>, Option<Box<OsStr>>) {
        match std::str::from_utf8(name) {
            Ok(text) => (text.into(), None),
            Err(_) => (
                String::from_utf8_lossy(name).into(),
                Some(OsStr::from_bytes(name).into()),
            ),
        }
    }

    fn from_stat(name: &[u8], stat: &rustix::fs::Stat) -> Self {
        let (name, exact) = Self::named(name);
        let kind = match FileType::from_raw_mode(stat.st_mode) {
            FileType::Directory => Kind::Directory,
            FileType::Symlink => Kind::Link,
            FileType::RegularFile => Kind::File,
            _ => Kind::Other,
        };
        Self {
            name,
            exact,
            kind,
            apparent: stat.st_size.max(0) as u64,
            allocated: (stat.st_blocks.max(0) as u64).saturating_mul(512),
            modified: stat.st_mtime,
            device: i64::from(stat.st_dev).cast_unsigned(),
            inode: stat.st_ino,
            links: u32::from(stat.st_nlink),
            flags: stat.st_flags,
            hints: None,
        }
    }
}

/// A regular file's clone facts from what its listing said, `read`
/// being the file's own metadata read: its private size, reference count
/// and, when the read found the file the listing did, its sharing flags.
/// [`Entry::clone_info`] calls it for regular files only, with the read
/// by path; the tests stand in for the read.
fn resolve(
    hints: Hints,
    inode: u64,
    links: u64,
    allocated: u64,
    cache: &crate::clone_cache::Cache,
    read: impl Fn() -> (CloneInfo, Option<u64>),
) -> Option<CloneInfo> {
    let Hints { device, id, flags } = hints;
    let mut measured = None;
    let flags = match flags {
        Some(flags) => flags,
        None if cache.shares_all(device, id) => MAY_SHARE | FULL_SHARE,
        None => {
            let (info, flags) = read();
            measured = Some(info);
            // A file that could not be read again may be a clone: its
            // private size stays unknown, the side that never overstates
            // a saving.
            flags.unwrap_or(MAY_SHARE)
        }
    };
    if flags & MAY_SHARE == 0 {
        return None;
    }
    let measure = || measured.unwrap_or_else(|| read().0);
    if flags & FULL_SHARE == 0 {
        return Some(measure());
    }
    // A resource fork can differ even when the data fork is cloned. Reuse
    // only when the listing agrees with the cached data allocation.
    Some(cache.get(device, id, inode, links, allocated, measure))
}

/// A directory being listed; like `std::fs::ReadDir`, it leaves out `.`
/// and `..`.
#[derive(Debug)]
pub struct ReadDir {
    fd: OwnedFd,
    /// `u64`s for alignment; the records themselves are 4-byte aligned.
    buffer: Option<Box<[u64]>>,
    /// Records in the buffer not yet handed out.
    remaining: u32,
    /// Where the next of them starts.
    offset: usize,
    done: bool,
    clones: Clones,
}

/// Directories listed without sharing flags, so a test can tell that its
/// own scan listed some: tests scan side by side.
#[cfg(test)]
static WITHOUT_FLAGS: std::sync::Mutex<Vec<PathBuf>> =
    std::sync::Mutex::new(Vec::new());

/// List `dir`, with each entry's size, identity and age. A link at `dir`
/// itself is followed, as `std::fs::read_dir` follows it.
pub fn read_dir(dir: &Path, clones: Clones) -> io::Result<ReadDir> {
    #[cfg(test)]
    if clones == Clones::Ids {
        WITHOUT_FLAGS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(dir.to_path_buf());
    }
    let fd = rustix::fs::open(
        dir,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let buffer = SPARE
        .with_borrow_mut(Option::take)
        .unwrap_or_else(|| vec![0; BUFFER_BYTES / 8].into_boxed_slice());
    Ok(ReadDir {
        fd,
        buffer: Some(buffer),
        remaining: 0,
        offset: 0,
        done: false,
        clones,
    })
}

impl Drop for ReadDir {
    fn drop(&mut self) {
        if let Some(buffer) = self.buffer.take() {
            SPARE.with_borrow_mut(|spare| *spare = Some(buffer));
        }
    }
}

impl Iterator for ReadDir {
    type Item = io::Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            if self.done {
                return None;
            }
            match self.fill() {
                Ok(0) => {
                    self.done = true;
                    return None;
                }
                Ok(count) => {
                    self.remaining = count;
                    self.offset = 0;
                }
                Err(error) => {
                    self.done = true;
                    return Some(Err(error));
                }
            }
        }
        self.remaining -= 1;
        match self.record() {
            Ok((entry, length)) => {
                self.offset += length;
                Some(entry)
            }
            Err(error) => {
                self.remaining = 0;
                self.done = true;
                Some(Err(error))
            }
        }
    }
}

impl ReadDir {
    /// Ask the kernel for the next buffer of records; how many it wrote.
    fn fill(&mut self) -> io::Result<u32> {
        let buffer = self.buffer.as_mut().expect("held until drop");
        let mut request = AttrList {
            bitmapcount: ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: COMMON_ATTRIBUTES,
            volattr: 0,
            dirattr: 0,
            fileattr: FILE_ATTRIBUTES,
            forkattr: self.clones.attributes(),
        };
        loop {
            // SAFETY: the descriptor is an open directory owned by `self`;
            // `request` is a valid attrlist for the call's duration; and the
            // pointer and length describe `buffer`, which is writable for
            // that many bytes and not otherwise borrowed during the call.
            let count = unsafe {
                getattrlistbulk(
                    self.fd.as_raw_fd(),
                    &raw mut request,
                    buffer.as_mut_ptr().cast(),
                    BUFFER_BYTES,
                    if self.clones == Clones::Off {
                        0
                    } else {
                        u64::from(EXTENDED)
                    },
                )
            };
            if let Ok(count) = u32::try_from(count) {
                return Ok(count);
            }
            let error = io::Error::last_os_error();
            // Optional clone hints must not disable ordinary enumeration on
            // volumes or kernels that reject extended attributes.
            if self.clones != Clones::Off
                && matches!(
                    error.raw_os_error(),
                    Some(libc::EINVAL | libc::ENOTSUP)
                )
            {
                self.clones = Clones::Off;
                request.forkattr = 0;
                continue;
            }
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// The buffer as the bytes the kernel wrote.
    fn bytes(&self) -> &[u8] {
        let buffer = self.buffer.as_deref().expect("held until drop");
        // SAFETY: `[u64]` can be read as `u8`s of eight times its length:
        // one allocation, no alignment requirement for `u8`, and every byte
        // initialized, since the buffer starts zeroed.
        unsafe {
            std::slice::from_raw_parts(
                buffer.as_ptr().cast::<u8>(),
                size_of_val(buffer),
            )
        }
    }

    /// The entry at `self.offset` and the record's length. Every length and
    /// offset the kernel wrote is checked against the buffer before it is
    /// used.
    ///
    /// Layout per getattrlist(2): a `u32` length, the returned
    /// `attribute_set_t`, `ATTR_CMN_ERROR` when it is set, then every other
    /// attribute in bit order, each packed at 4-byte alignment.
    fn record(&self) -> io::Result<(io::Result<Entry>, usize)> {
        let bytes = self.bytes();
        let length = read_u32(bytes, self.offset)? as usize;
        let record = bytes
            .get(self.offset..self.offset + length)
            .ok_or_else(malformed)?;
        let mut cursor = Cursor { record, at: 4 };
        let common = cursor.u32()?;
        cursor.at += 8;
        let file = cursor.u32()?;
        let extended = cursor.u32()?;
        let error = if common & ATTR_CMN_ERROR == 0 {
            0
        } else {
            cursor.u32()?
        };
        let name = cursor.name()?;
        if error != 0 {
            let error = io::Error::from_raw_os_error(error.cast_signed());
            return Ok((Err(error), length));
        }
        let entry = if common & MEASURED == MEASURED {
            cursor.entry(file, extended, self.clones, name)?
        } else {
            None
        };
        Ok((entry.map_or_else(|| self.stat(name), Ok), length))
    }

    /// An entry the file system described only in part, read the ordinary
    /// way, relative to the directory already open.
    fn stat(&self, name: &[u8]) -> io::Result<Entry> {
        let stat =
            rustix::fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW)?;
        Ok(Entry::from_stat(name, &stat))
    }
}

fn malformed() -> io::Error {
    io::Error::other("the kernel returned a malformed record")
}

fn read_u32(bytes: &[u8], at: usize) -> io::Result<u32> {
    let field = at
        .checked_add(4)
        .and_then(|end| bytes.get(at..end))
        .ok_or_else(malformed)?;
    Ok(u32::from_ne_bytes(
        field.try_into().map_err(|_| malformed())?,
    ))
}

/// Reads a record's attributes in order; reading past its end is a
/// malformed record.
struct Cursor<'a> {
    record: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn u32(&mut self) -> io::Result<u32> {
        let value = read_u32(self.record, self.at)?;
        self.at += 4;
        Ok(value)
    }

    fn u64(&mut self) -> io::Result<u64> {
        let field = self
            .at
            .checked_add(8)
            .and_then(|end| self.record.get(self.at..end))
            .ok_or_else(malformed)?;
        self.at += 8;
        Ok(u64::from_ne_bytes(
            field.try_into().map_err(|_| malformed())?,
        ))
    }

    /// An `attrreference_t`: an offset from itself and a length that counts
    /// the terminating NUL.
    fn name(&mut self) -> io::Result<&'a [u8]> {
        let reference = self.at;
        let offset = isize::try_from(self.u32()?.cast_signed())
            .map_err(|_| malformed())?;
        let length = self.u32()? as usize;
        let name = reference
            .checked_add_signed(offset)
            .zip(length.checked_sub(1))
            .and_then(|(start, length)| {
                self.record.get(start..start.checked_add(length)?)
            })
            .ok_or_else(malformed)?;
        if name.is_empty() || name == b"." || name == b".." || name.contains(&0)
        {
            return Err(malformed());
        }
        Ok(name)
    }

    /// The attributes after the name, every one of them asked for; `None`
    /// for a file whose sizes the file system left out.
    fn entry(
        &mut self,
        file: u32,
        extended: u32,
        clones: Clones,
        name: &[u8],
    ) -> io::Result<Option<Entry>> {
        let device = self.u32()?.cast_signed();
        let kind = match self.u32()? {
            VDIR => Kind::Directory,
            VLNK => Kind::Link,
            VREG => Kind::File,
            _ => Kind::Other,
        };
        let modified = self.u64()?.cast_signed();
        self.at += 8;
        let flags = self.u32()?;
        let inode = self.u64()?;
        // A directory is stated the ordinary way. A mount point is listed
        // with the attributes of the directory it covers, so its DEVID is
        // the parent volume's, and the walk would cross into every disk,
        // share and system volume mounted below the root. `statat` reads
        // the mounted root, as `lstat` did; directories are few beside files.
        let (links, allocated, apparent) = if kind == Kind::Directory {
            return Ok(None);
        } else if file & FILE_ATTRIBUTES == FILE_ATTRIBUTES {
            (self.u32()?, self.u64()?, self.u64()?)
        } else {
            return Ok(None);
        };
        let real_device = if extended & REAL_DEVICE != 0 {
            u64::from(self.u32()?)
        } else {
            0
        };
        let clone_id = if extended & libc::ATTR_CMNEXT_CLONEID != 0 {
            Some(self.u64()?)
        } else {
            None
        };
        let clone_flags = if extended & libc::ATTR_CMNEXT_EXT_FLAGS != 0 {
            Some(self.u64()?)
        } else {
            None
        };
        // Flags that were asked for and not given mean no clone facts, as
        // they always have; flags not asked for come from the group.
        let hints = clone_id
            .filter(|_| clones == Clones::Ids || clone_flags.is_some())
            .map(|id| Hints {
                device: real_device,
                id,
                flags: clone_flags,
            });
        let (name, exact) = Entry::named(name);
        Ok(Some(Entry {
            name,
            exact,
            kind,
            apparent,
            allocated,
            modified,
            device: i64::from(device).cast_unsigned(),
            inode,
            links,
            flags,
            hints,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn listed(dir: &Path) -> Vec<Entry> {
        let mut entries: Vec<Entry> = read_dir(dir, Clones::Off)
            .expect("list")
            .collect::<io::Result<_>>()
            .expect("entries");
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        entries
    }

    /// What `lstat` says of each entry, the way the standard listing reads it.
    fn stated(dir: &Path) -> Vec<Entry> {
        let mut entries: Vec<Entry> = fs::read_dir(dir)
            .expect("list")
            .map(|entry| {
                let entry = entry.expect("entry");
                let stat = rustix::fs::lstat(entry.path()).expect("lstat");
                Entry::from_stat(entry.file_name().as_bytes(), &stat)
            })
            .collect();
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        entries
    }

    type Fields = (String, Kind, u64, u64, i64, (u64, u64), bool, bool);

    /// What the walk reads of a leaf.
    fn fields(entry: &Entry) -> Fields {
        (
            entry.name.to_string(),
            entry.kind,
            entry.apparent,
            entry.allocated,
            entry.modified,
            entry.identity(),
            entry.shared(),
            entry.evicted(),
        )
    }

    #[test]
    fn a_listing_has_names_kinds_lengths_and_no_dot_entries() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("sub")).expect("mkdir");
        fs::write(temp.path().join("data.bin"), vec![7_u8; 100_000])
            .expect("write");
        symlink("data.bin", temp.path().join("link")).expect("symlink");

        let entries = listed(temp.path());
        let names: Vec<&OsStr> = entries.iter().map(Entry::file_name).collect();
        assert_eq!(names, ["data.bin", "link", "sub"]);
        assert_eq!(entries[0].kind(), Kind::File);
        assert_eq!(entries[0].apparent(), 100_000);
        assert_eq!(entries[0].path(temp.path()), temp.path().join("data.bin"));
        assert_eq!(entries[1].kind(), Kind::Link);
        assert_eq!(entries[2].kind(), Kind::Directory);
        assert!(entries[0].modified() > 0, "written just now");
    }

    #[test]
    fn every_leaf_reads_as_lstat_reads_it() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        fs::write(root.join("plain"), b"hi").expect("write");
        fs::hard_link(root.join("plain"), root.join("hard")).expect("link");
        // Ten megabytes long, nothing written: no blocks.
        fs::File::create(root.join("sparse"))
            .and_then(|file| file.set_len(10 << 20))
            .expect("sparse");
        symlink("plain", root.join("link")).expect("symlink");
        // A resource fork, which `st_blocks` counts and `st_size` does not.
        let forked = root.join("forked");
        fs::write(&forked, b"data").expect("write");
        rustix::fs::setxattr(
            &forked,
            "com.apple.ResourceFork",
            &vec![b'r'; 20_000],
            rustix::fs::XattrFlags::empty(),
        )
        .expect("resource fork");

        let listed: Vec<Fields> = listed(root).iter().map(fields).collect();
        let stated: Vec<Fields> = stated(root).iter().map(fields).collect();
        assert_eq!(listed, stated);
        // Adding clone hints must preserve ordinary metadata, including
        // resource forks and hardlink identity in the same bulk record.
        let mut clone_entries: Vec<Entry> = read_dir(root, Clones::Flags)
            .expect("list with clone hints")
            .collect::<io::Result<_>>()
            .expect("entries");
        clone_entries.sort_by(|left, right| left.name.cmp(&right.name));
        let clone_fields: Vec<Fields> =
            clone_entries.iter().map(fields).collect();
        assert_eq!(clone_fields, stated);
        let forked = listed.iter().find(|leaf| leaf.0 == "forked").unwrap();
        assert_eq!(forked.2, 4, "the data fork's length");
        assert!(forked.3 >= 20_000, "the fork's blocks: {}", forked.3);
        let plain = listed.iter().find(|leaf| leaf.0 == "plain").unwrap();
        assert!(plain.6, "two names");
    }

    #[test]
    fn a_directory_larger_than_the_buffer_is_listed_whole() {
        let temp = TempDir::new().expect("tempdir");
        // About a hundred bytes a record: several fills.
        let count = 3 * BUFFER_BYTES / 100;
        for index in 0..count {
            fs::write(temp.path().join(format!("file-{index:05}")), b"")
                .expect("write");
        }
        let entries = listed(temp.path());
        assert_eq!(entries.len(), count);
        assert_eq!(entries[0].name(), "file-00000");
        assert_eq!(entries[count - 1].name(), format!("file-{:05}", count - 1));
    }

    #[test]
    fn names_are_text_and_open() {
        let temp = TempDir::new().expect("tempdir");
        for name in ["café", "日本語", "with space", "a\nb"] {
            fs::write(temp.path().join(name), b"x").expect("write");
        }
        let entries = listed(temp.path());
        assert_eq!(entries.len(), 4);
        for entry in entries {
            assert!(entry.exact.is_none(), "{}", entry.name());
            assert!(fs::metadata(entry.path(temp.path())).is_ok());
        }
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        let temp = TempDir::new().expect("tempdir");
        let error = read_dir(&temp.path().join("gone"), Clones::Off)
            .expect_err("missing");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}

const fn clone_attrs(common: u32, file: u32, extended: u32) -> libc::attrlist {
    libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: RETURNED | common,
        volattr: 0,
        dirattr: 0,
        fileattr: file,
        forkattr: extended,
    }
}

/// A clone's private size and reference count, read again by path, and
/// its sharing flags when the read found the file the listing did.
fn private_info(
    path: &Path,
    device: u64,
    inode: u64,
    id: u64,
) -> (CloneInfo, Option<u64>) {
    let mut info = CloneInfo {
        device,
        id,
        ..CloneInfo::default()
    };
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return (info, None);
    };
    // Re-read identity alongside private size: a replacement between the
    // listing/stat and this call must not receive the old file's estimate.
    let mut request = clone_attrs(
        libc::ATTR_CMN_FILEID,
        libc::ATTR_FILE_DATAALLOCSIZE,
        libc::ATTR_CMNEXT_PRIVATESIZE | HINTS | REFCNT,
    );
    let mut buffer = [0_u8; 96];
    loop {
        // SAFETY: valid NUL-terminated path, initialized ABI request and
        // writable buffer. NOFOLLOW never asks a symlink target for data.
        let result = unsafe {
            libc::getattrlist(
                path.as_ptr(),
                (&raw mut request).cast(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                EXTENDED | libc::FSOPT_NOFOLLOW | libc::FSOPT_PACK_INVAL_ATTRS,
            )
        };
        if result == 0 {
            break;
        }
        if io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL)
            && request.forkattr & REFCNT != 0
        {
            request.forkattr &= !REFCNT;
            continue;
        }
        return (info, None);
    }
    let Some(length) = clone_read_u32(&buffer, 0) else {
        return (info, None);
    };
    let Some(buffer) = buffer.get(..length as usize) else {
        return (info, None);
    };
    let common = clone_read_u32(buffer, 4).unwrap_or(0);
    let files = clone_read_u32(buffer, 16).unwrap_or(0);
    let extended = clone_read_u32(buffer, 20).unwrap_or(0);
    if common & libc::ATTR_CMN_FILEID == 0
        || clone_read_u64(buffer, 24) != Some(inode)
    {
        return (info, None);
    }
    // Packed order includes realdevid before cloneid, even when unavailable.
    // PACK_INVAL_ATTRS reserves its slot; ordinary st_dev can alias APFS
    // System and Data volumes and must never substitute for this identity.
    let real_device = if extended & REAL_DEVICE != 0 {
        u64::from(clone_read_u32(buffer, 48).unwrap_or(0))
    } else {
        0
    };
    if device != 0 && device != real_device {
        return (info, None);
    }
    info.device = real_device;
    if extended & libc::ATTR_CMNEXT_CLONEID == 0
        || clone_read_u64(buffer, 52) != Some(id)
    {
        return (info, None);
    }
    if extended & libc::ATTR_CMNEXT_PRIVATESIZE != 0 {
        info.private_bytes = clone_read_u64(buffer, 40);
    }
    if files & libc::ATTR_FILE_DATAALLOCSIZE != 0 {
        info.data_bytes = clone_read_u64(buffer, 32).unwrap_or(0);
    }
    if extended & REFCNT != 0 {
        info.references = clone_read_u32(buffer, 68).unwrap_or(0);
    }
    let flags = if extended & libc::ATTR_CMNEXT_EXT_FLAGS == 0 {
        None
    } else {
        clone_read_u64(buffer, 60)
    };
    (info, flags)
}

fn clone_read_u32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}
fn clone_read_u64(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_ne_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

/// Exceptional path for followed symlinks; ordinary entries use bulk hints.
pub fn clone_info(path: &Path, meta: &Metadata) -> Option<CloneInfo> {
    let path_c = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut request = clone_attrs(0, 0, HINTS);
    let mut buffer = [0_u8; 48];
    // SAFETY: valid path, ABI request and writable buffer of the stated size.
    let result = unsafe {
        libc::getattrlist(
            path_c.as_ptr(),
            (&raw mut request).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            EXTENDED | libc::FSOPT_NOFOLLOW | libc::FSOPT_PACK_INVAL_ATTRS,
        )
    };
    if result != 0
        || clone_read_u32(&buffer, 20)? & HINTS != HINTS
        || clone_read_u64(&buffer, 36)? & MAY_SHARE == 0
    {
        return None;
    }
    Some(
        private_info(
            path,
            u64::from(clone_read_u32(&buffer, 24)?),
            meta.ino(),
            clone_read_u64(&buffer, 28)?,
        )
        .0,
    )
}

#[cfg(test)]
mod clone_tests {
    use super::*;
    use crate::scan::{ScanOptions, scan};
    use std::fs;
    use std::io::{Seek, SeekFrom, Write};
    use std::process::Command;

    fn fixture() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("original");
        fs::write(&source, vec![0x5a; 8 * 1024 * 1024]).expect("write");
        assert!(
            Command::new("cp")
                .arg("-c")
                .arg(&source)
                .arg(temp.path().join("clone"))
                .status()
                .expect("cp -c")
                .success()
        );
        temp
    }

    #[test]
    fn ten_apfs_worktrees_with_a_surviving_clone_only_credit_private_files() {
        let temp = fixture();
        for index in 0..10 {
            let dir = temp.path().join(format!("worktree-{index}"));
            fs::create_dir(&dir).expect("worktree");
            assert!(
                Command::new("cp")
                    .arg("-c")
                    .arg(temp.path().join("original"))
                    .arg(dir.join("shared"))
                    .status()
                    .expect("cp -c")
                    .success()
            );
            fs::write(dir.join("private"), vec![0x31; 64 * 1024])
                .expect("private file");
        }
        let tree = scan(temp.path(), ScanOptions::default()).expect("scan");
        let tree = tree.root();
        let selected: Vec<_> = tree
            .children()
            .iter()
            .filter(|node| node.name().starts_with("worktree-"))
            .collect();
        assert_eq!(selected.len(), 10);
        assert_eq!(
            selected.iter().map(|node| node.bytes).sum::<u64>(),
            10 * (8 * 1024 * 1024 + 64 * 1024)
        );
        let estimate = crate::sharing::estimate(selected);
        assert_eq!(estimate.lower, 10 * 64 * 1024);
        assert_eq!(estimate.upper, estimate.lower);
        assert!(!estimate.partial);
    }

    #[test]
    fn a_listing_without_flags_finds_what_one_with_them_finds() {
        let temp = fixture();
        let dir = temp.path();
        for name in ["second", "modified"] {
            assert!(
                Command::new("cp")
                    .arg("-c")
                    .arg(dir.join("original"))
                    .arg(dir.join(name))
                    .status()
                    .expect("cp -c")
                    .success()
            );
        }
        let mut modified = fs::OpenOptions::new()
            .write(true)
            .open(dir.join("modified"))
            .expect("open");
        modified.write_all(&[1; 4096]).expect("write");
        modified.sync_all().expect("sync");
        fs::write(dir.join("plain"), vec![7; 64 * 1024]).expect("plain");
        fs::hard_link(dir.join("second"), dir.join("linked")).expect("link");
        let infos = |clones| {
            let cache = crate::clone_cache::Cache::default();
            let mut entries: Vec<Entry> = read_dir(dir, clones)
                .expect("list")
                .collect::<io::Result<_>>()
                .expect("entries");
            entries.sort_by(|left, right| left.name.cmp(&right.name));
            if clones == Clones::Ids {
                assert!(entries.iter().all(|entry| {
                    entry.hints.is_some_and(|hints| hints.flags.is_none())
                }));
            }
            entries
                .iter()
                .map(|entry| {
                    (entry.name.clone(), entry.clone_info(dir, &cache))
                })
                .collect::<Vec<_>>()
        };
        let flagged = infos(Clones::Flags);
        let shared: Vec<&str> = flagged
            .iter()
            .filter(|(_, info)| info.is_some())
            .map(|(name, _)| &**name)
            .collect();
        assert_eq!(
            shared,
            ["clone", "linked", "modified", "original", "second"]
        );
        assert_eq!(infos(Clones::Ids), flagged);
    }

    /// Every node's name, size, allocation and sharing figures, children
    /// by name: two scans of one tree read the same only if all agree.
    fn every_figure(node: crate::tree::Node<'_>, into: &mut Vec<String>) {
        into.push(format!(
            "{} {} {} {:?} {:?}",
            node.name(),
            node.bytes,
            node.files,
            node.allocation(),
            node.sharing()
        ));
        let mut children: Vec<_> = node.children().iter().collect();
        children.sort_by(|left, right| left.name().cmp(right.name()));
        for child in children {
            every_figure(child, into);
        }
    }

    /// Worktrees cloned from a source tree, as an agent makes them: some
    /// with a modified clone, a file of their own, a hardlink, a resource
    /// fork, and clones at the top that put their siblings' listings
    /// without flags. Scanned as listings choose and with flags in every
    /// listing, every figure agrees.
    #[test]
    fn worktrees_scan_the_same_with_flags_in_every_listing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let source = root.join("source");
        fs::create_dir_all(source.join("nested/deeper")).expect("source");
        for (name, size) in [
            ("a", 64 * 1024),
            ("b", 8 * 1024),
            ("nested/c", 16 * 1024),
            ("nested/deeper/d", 4096),
            ("nested/deeper/e", 12 * 1024),
        ] {
            fs::write(source.join(name), vec![name.len() as u8; size])
                .expect("source file");
        }
        let clone = |from: &Path, to: &Path| {
            assert!(
                Command::new("cp")
                    .arg("-c")
                    .arg("-R")
                    .arg(from)
                    .arg(to)
                    .status()
                    .expect("cp -c")
                    .success()
            );
        };
        for index in 0..6 {
            clone(&source, &root.join(format!("worktree-{index}")));
        }
        for name in ["top-a", "top-b"] {
            clone(&source.join("a"), &root.join(name));
        }
        let mut modified = fs::OpenOptions::new()
            .write(true)
            .open(root.join("worktree-1/nested/c"))
            .expect("open");
        modified.write_all(&[9; 4096]).expect("write");
        modified.sync_all().expect("sync");
        fs::write(root.join("worktree-2/nested/own"), vec![3; 20 * 1024])
            .expect("own file");
        fs::hard_link(
            root.join("worktree-3/b"),
            root.join("worktree-3/nested/linked"),
        )
        .expect("hardlink");
        fs::write(
            root.join("worktree-4/nested/deeper/d/..namedfork/rsrc"),
            [1; 4096],
        )
        .expect("resource fork");
        let scanned = || {
            let tree = scan(
                root,
                ScanOptions {
                    // Which name of a hardlink is charged depends on the
                    // order workers meet them.
                    dedup_hardlinks: false,
                    ..ScanOptions::default()
                },
            )
            .expect("scan");
            let mut figures = Vec::new();
            every_figure(tree.root(), &mut figures);
            figures
        };
        let chosen = scanned();
        // The walk lists the root as it resolves: /var is /private/var.
        let real = fs::canonicalize(root).expect("canonical root");
        assert!(
            WITHOUT_FLAGS
                .lock()
                .expect("listings")
                .iter()
                .any(|dir| dir.starts_with(root) || dir.starts_with(&real)),
            "some listing of this tree left the flags out"
        );
        let flagged = {
            /// Clears the switch even if the scan panics, so no later test
            /// in this process scans with it set.
            struct Reset;
            impl Drop for Reset {
                fn drop(&mut self) {
                    crate::scan::ALWAYS_FLAGS
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                }
            }
            crate::scan::ALWAYS_FLAGS
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let _reset = Reset;
            scanned()
        };
        assert_eq!(chosen, flagged);
        // The clones were seen as clones: the copies repeat bytes.
        let root = &chosen[0];
        assert!(!root.contains("duplicate_bytes: 0,"), "{root}");
    }

    #[test]
    fn real_clones_keep_listed_bytes_and_report_sharing() {
        let temp = fixture();
        let tree = scan(temp.path(), ScanOptions::default()).expect("scan");
        let tree = tree.root();
        assert_eq!(tree.bytes, 16 * 1024 * 1024);
        assert_eq!(tree.sharing().files, 2, "{tree:#?}");
        assert_eq!(tree.sharing().bytes, tree.bytes);
        assert_eq!(tree.sharing().duplicate_bytes, 8 * 1024 * 1024);
        assert_eq!(tree.sharing().private_bytes, 0);
        assert_eq!(tree.sharing().unknown_files, 0);
        let baseline = scan(
            temp.path(),
            ScanOptions {
                apfs_clone_metadata: false,
                ..ScanOptions::default()
            },
        )
        .expect("scan without clone metadata");
        let baseline = baseline.root();
        assert_eq!(baseline.bytes, tree.bytes);
        assert_eq!(baseline.sharing().files, 0);
        let apparent = scan(
            temp.path(),
            ScanOptions {
                apparent_size: true,
                ..ScanOptions::default()
            },
        )
        .expect("apparent scan");
        assert_eq!(apparent.root().sharing().files, 0);
    }

    #[test]
    fn real_clone_hardlinks_and_followed_names_have_set_level_savings() {
        let temp = fixture();
        let original = temp.path().join("original");
        fs::hard_link(&original, temp.path().join("alias")).expect("hardlink");
        std::os::unix::fs::symlink(&original, temp.path().join("followed"))
            .expect("symlink");
        let tree = scan(
            temp.path(),
            ScanOptions {
                follow_links: true,
                ..ScanOptions::default()
            },
        )
        .expect("scan");
        let tree = tree.root();
        let by_name = |name: &str| {
            tree.children()
                .iter()
                .find(|node| node.name() == name)
                .expect("entry")
        };
        let original = by_name("original");
        let alias = by_name("alias");
        let clone = by_name("clone");
        let followed = by_name("followed");
        for node in [original, alias, clone, followed] {
            assert_eq!(crate::sharing::estimate([node]).lower, 0);
        }
        assert_eq!(crate::sharing::estimate([original, clone]).lower, 0);
        assert_eq!(
            crate::sharing::estimate([original, alias, clone]).lower,
            8 * 1024 * 1024
        );
        assert_eq!(tree.sharing().reclaimable(tree.bytes), 8 * 1024 * 1024);
        for node in [original, alias, clone] {
            assert_ne!(
                node.allocation()
                    .expect("allocation")
                    .clone
                    .expect("clone")
                    .device,
                0
            );
        }
    }

    #[test]
    fn resource_fork_allocation_remains_in_the_range_for_full_clones() {
        let temp = fixture();
        let fork = temp.path().join("clone/..namedfork/rsrc");
        fs::write(fork, vec![0x31; 4096]).expect("resource fork");
        let tree = scan(temp.path(), ScanOptions::default()).expect("scan");
        let tree = tree.root();
        assert_eq!(tree.sharing().private_bytes, 0);
        assert_eq!(tree.sharing().reclaimable(tree.bytes), 8 * 1024 * 1024);
        assert_eq!(tree.sharing().possible(tree.bytes), 8 * 1024 * 1024 + 4096);
        assert_eq!(tree.sharing().duplicate_bytes, 8 * 1024 * 1024);
        let clone = tree
            .children()
            .iter()
            .find(|node| node.name() == "clone")
            .expect("clone");
        assert_eq!(clone.sharing().reclaimable(clone.bytes), 0);
        assert_eq!(clone.sharing().possible(clone.bytes), 4096);
    }

    #[test]
    fn modified_clones_keep_private_bytes_and_hardlinks_are_not_clones() {
        let temp = fixture();
        let clone = temp.path().join("clone");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&clone)
            .expect("open");
        file.seek(SeekFrom::Start(1024 * 1024)).expect("seek");
        file.write_all(&[1; 4096]).expect("write");
        file.sync_all().expect("sync");
        fs::hard_link(&clone, temp.path().join("linked")).expect("hardlink");
        std::os::unix::fs::symlink(&clone, temp.path().join("symlink"))
            .expect("symlink");
        for follow_links in [false, true] {
            let tree = scan(
                temp.path(),
                ScanOptions {
                    follow_links,
                    ..ScanOptions::default()
                },
            )
            .expect("scan");
            let tree = tree.root();
            assert_eq!(tree.sharing().files, 2, "{tree:#?}");
            assert!(tree.sharing().private_bytes >= 4096);
            assert!(tree.sharing().private_bytes < tree.sharing().bytes);
            assert_eq!(tree.sharing().duplicate_bytes, 0);
        }
    }

    #[test]
    fn unreadable_private_metadata_is_unknown_not_full_allocation() {
        let temp = fixture();
        let source = temp.path().join("original");
        let meta = fs::metadata(&source).expect("metadata");
        let (info, flags) = private_info(
            &temp.path().join("missing"),
            meta.dev(),
            meta.ino(),
            42,
        );
        assert_eq!(flags, None);
        assert_eq!(info.private_bytes, None);
        assert_eq!(info.references, 0);
    }

    #[test]
    fn malformed_bulk_names_and_records_are_rejected() {
        assert!(
            Cursor {
                record: &[],
                at: 24
            }
            .name()
            .is_err()
        );
        let mut record = vec![0_u8; 64];
        record[4..8]
            .copy_from_slice(&(RETURNED | libc::ATTR_CMN_NAME).to_ne_bytes());
        record[24..28].copy_from_slice(&8_u32.to_ne_bytes());
        record[28..32].copy_from_slice(&3_u32.to_ne_bytes());
        record[32..35].copy_from_slice(b"ok\0");
        assert_eq!(
            Cursor {
                record: &record,
                at: 24
            }
            .name()
            .expect("name"),
            b"ok"
        );
        record[32..35].copy_from_slice(b"..\0");
        assert!(
            Cursor {
                record: &record,
                at: 24
            }
            .name()
            .is_err()
        );
        record[24..28].copy_from_slice(&u32::MAX.to_ne_bytes());
        assert!(
            Cursor {
                record: &record,
                at: 24
            }
            .name()
            .is_err()
        );
    }
}

/// A model of APFS clones checked against [`resolve`]: listing without
/// sharing flags must find what listing with them finds.
#[cfg(test)]
mod resolve_tests {
    use super::*;
    use crate::clone_cache::Cache;
    use crate::test_rng::Rng;
    use std::cell::Cell;

    /// One file as the volume knows it.
    #[derive(Clone, Copy, Debug)]
    struct File {
        device: u64,
        id: u64,
        inode: u64,
        links: u64,
        /// Every fork's allocation, as the listing reports it.
        allocated: u64,
        flags: u64,
        /// What the file's own metadata read finds; `None` flags for a
        /// file gone since it was listed.
        read: (CloneInfo, Option<u64>),
    }

    /// Clone groups as APFS keeps them: files of a group share one data
    /// stream, so its reference count and whether it is shared are the
    /// group's; whether a file may share is its own, and set on every
    /// file whose stream is shared. Some files are hardlinked, some have
    /// a resource fork the group does not share, some are gone by the
    /// time they are read, and some groups are on a volume of no known
    /// identity.
    fn world(rng: &mut Rng) -> Vec<File> {
        let mut files = Vec::new();
        let mut inode = 100;
        for id in 1..=rng.below(12) + 1 {
            let device = u64::from(!rng.percent(10));
            let data = 4096 * (rng.below(4) + 1);
            let kind = rng.below(3);
            let (references, flags, private, members) = match kind {
                // A stream shared by files in the scan and maybe outside.
                0 => {
                    let members = rng.below(5) + 1;
                    let outside = rng.below(3);
                    let references =
                        u32::try_from(members + outside).expect("small");
                    (references.max(2), MAY_SHARE | FULL_SHARE, 0, members)
                }
                // Cloned once, now alone: a modified clone or an original
                // whose clones are gone.
                1 => (1, 0x1, rng.below(2) * 4096, 1),
                // Never cloned.
                _ => (1, 0, data, 1),
            };
            for _ in 0..members {
                inode += 1;
                let fork = if rng.percent(10) { 4096 } else { 0 };
                let links = if rng.percent(15) { 2 } else { 1 };
                let gone = rng.percent(5);
                let info = CloneInfo {
                    device,
                    id,
                    references,
                    private_bytes: (!gone).then_some(private),
                    data_bytes: data,
                };
                let file = File {
                    device,
                    id,
                    inode,
                    links,
                    allocated: data + fork,
                    flags,
                    read: (info, (!gone).then_some(flags)),
                };
                for _ in 0..links {
                    files.push(file);
                }
            }
        }
        rng.shuffle(&mut files);
        files
    }

    /// Each file resolved in order with one cache, as a scan's listings
    /// are: with the listing's flags or without; and the reads made, for
    /// files that may share and files that never could.
    fn run(files: &[File], flags: bool) -> (Vec<Option<CloneInfo>>, u64, u64) {
        let cache = Cache::default();
        let (mut sharing, mut plain) = (0, 0);
        let found = files
            .iter()
            .map(|file| {
                let reads = Cell::new(0);
                let hints = Hints {
                    device: file.device,
                    id: file.id,
                    flags: flags.then_some(file.flags),
                };
                let found = resolve(
                    hints,
                    file.inode,
                    file.links,
                    file.allocated,
                    &cache,
                    || {
                        reads.set(reads.get() + 1);
                        file.read
                    },
                );
                if file.flags & MAY_SHARE == 0 {
                    plain += reads.get();
                } else {
                    sharing += reads.get();
                }
                found
            })
            .collect();
        (found, sharing, plain)
    }

    #[test]
    fn a_listing_without_flags_resolves_every_file_as_one_with_them() {
        for seed in 0..2000 {
            let files = world(&mut Rng::new(seed));
            let (listed, listed_reads, never) = run(&files, true);
            let (grouped, grouped_reads, plain_reads) = run(&files, false);
            assert_eq!(never, 0, "seed {seed}");
            for ((file, listed), grouped) in
                files.iter().zip(&listed).zip(&grouped)
            {
                if file.flags & MAY_SHARE == 0 && file.read.1.is_none() {
                    // Gone before it was read, and never a clone: without
                    // the listing's word for it, it counts as a clone of
                    // unknown size, which overstates no saving.
                    assert_eq!(*listed, None, "seed {seed}");
                    let grouped = grouped.expect("counted as a clone");
                    assert_eq!(grouped.private_bytes, None, "seed {seed}");
                } else {
                    assert_eq!(grouped, listed, "seed {seed}: {file:?}");
                }
            }
            // Files that may share are read no more often; a file never
            // cloned is read once a name, which the listing policy weighs.
            assert_eq!(grouped_reads, listed_reads, "seed {seed}");
            let plain = files.iter().filter(|f| f.flags & MAY_SHARE == 0);
            assert_eq!(plain_reads, plain.count() as u64, "seed {seed}");
        }
    }
}

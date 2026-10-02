//! What a walk on macOS needs beyond the standard library.
//!
//! `std::fs::read_dir` lists a directory with `readdir`, whose records carry
//! a name and a type, and `DirEntry::metadata` is then an `lstat` of each
//! entry's full path. `getattrlistbulk(2)` returns every entry's size,
//! identity, age and flags for a whole buffer of entries, without the kernel
//! setting up per-file state for each one as a stat must.

#![allow(
    unsafe_code,
    reason = "getattrlistbulk, which neither std nor rustix wraps; each block \
              says why it is sound"
)]

use std::cell::RefCell;
use std::ffi::{OsStr, c_int, c_void};
use std::io;
use std::os::fd::{AsRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};

/// Bytes the kernel fills per call: some hundreds of entries.
const BUFFER_BYTES: usize = 64 * 1024;

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

    /// The name, moved out: the tree keeps it, so nothing copies it. The
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
    pub const fn identity(&self) -> (u64, u64) {
        (self.device, self.inode)
    }

    pub const fn device(&self) -> u64 {
        self.device
    }

    /// Whether the file has more than one name.
    pub const fn shared(&self) -> bool {
        self.links > 1
    }

    /// Whether this is a directory iCloud Drive or a File Provider has
    /// evicted, which listing would make it fetch.
    pub const fn evicted(&self) -> bool {
        matches!(self.kind, Kind::Directory) && self.flags & SF_DATALESS != 0
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
        }
    }
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
}

/// List `dir`, with each entry's size, identity and age. A link at `dir`
/// itself is followed, as `std::fs::read_dir` follows it.
pub fn read_dir(dir: &Path) -> io::Result<ReadDir> {
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
            forkattr: 0,
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
                    0,
                )
            };
            if let Ok(count) = u32::try_from(count) {
                return Ok(count);
            }
            let error = io::Error::last_os_error();
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
        cursor.at += 4;
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
            cursor.entry(file, name)?
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
        reference
            .checked_add_signed(offset)
            .zip(length.checked_sub(1))
            .and_then(|(start, length)| {
                self.record.get(start..start.checked_add(length)?)
            })
            .ok_or_else(malformed)
    }

    /// The attributes after the name, every one of them asked for; `None`
    /// for a file whose sizes the file system left out.
    fn entry(&mut self, file: u32, name: &[u8]) -> io::Result<Option<Entry>> {
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
        // A directory's own size is never measured; its entries are.
        let (links, allocated, apparent) = if kind == Kind::Directory {
            (1, 0, 0)
        } else if file & FILE_ATTRIBUTES == FILE_ATTRIBUTES {
            (self.u32()?, self.u64()?, self.u64()?)
        } else {
            return Ok(None);
        };
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
        let mut entries: Vec<Entry> = read_dir(dir)
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
        let error = read_dir(&temp.path().join("gone")).expect_err("missing");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}

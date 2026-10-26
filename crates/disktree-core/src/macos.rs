//! What a walk on macOS needs beyond the standard library.
//!
//! `std::fs::read_dir` lists a directory with `readdir`, whose records carry
//! a name and a type, and `DirEntry::metadata` is then an `lstat` of each
//! entry's full path. `getattrlistbulk(2)` returns every entry's size,
//! identity, age and flags for a whole buffer of entries, without the kernel
//! setting up per-file state for each one as a stat must.
//!
//! It also says what a stat cannot: how much of a file APFS shares with
//! other files. A clone (`cp -c`, Finder's Duplicate) reports every block
//! it shares as its own in `st_blocks`, so three copies of 200 MB read as
//! 600 MB while the disk spends 200 MB, and deleting one gives back
//! nothing. The listing asks for each file's clone family and whether it
//! shares any blocks, so a total can charge the family once and a
//! projection can count only what deleting is sure to free.

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
use std::sync::atomic::{AtomicUsize, Ordering};

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
/// Lets the fork attribute word carry `ATTR_CMNEXT_*` attributes.
const FSOPT_ATTR_CMN_EXTENDED: u64 = 0x0000_0020;
/// Read a link at the path itself, not what it leads to.
const FSOPT_NOFOLLOW: u64 = 0x0000_0001;
/// Bytes of the file no other file shares: what deleting it alone is sure
/// to give back. Never asked of a whole listing: APFS walks the file's
/// extents to answer, which doubled the time to list a 3.1 million file
/// `~/Library` (15 s to 31 s, 8 workers). Asked of the few files that
/// share only some of their blocks, which no cheaper attribute measures.
const ATTR_CMNEXT_PRIVATESIZE: u32 = 0x0000_0008;
/// The clone family: every file cloned from one another, unchanged since,
/// has the same one. A write gives the written file a family of its own.
const ATTR_CMNEXT_CLONEID: u32 = 0x0000_0100;
/// `EF_*` flags from <sys/stat.h>, as a `u64`.
const ATTR_CMNEXT_EXT_FLAGS: u32 = 0x0000_0200;
/// How many files are in the clone family.
const ATTR_CMNEXT_CLONE_REFCNT: u32 = 0x0000_1000;
/// Below this, a file that shares some of its blocks is not asked how many
/// are its own, and claims none. In a 3.1 million file `~/Library`, 203,000
/// files shared some of their blocks; 195,000 of them were at most 64 KiB
/// and held 1.4 GB between them, while the 8,000 larger ones held 17 GB.
/// Asking all of them cost a 15 s scan 2 to 6 s more.
const PRIVATE_SIZE_FLOOR: u64 = 64 << 10;
/// The file shares, or once shared, blocks with another: it was cloned.
const EF_MAY_SHARE_BLOCKS: u64 = 0x0000_0001;
/// The file shares every block it has.
const EF_SHARES_ALL_BLOCKS: u64 = 0x0000_0040;
/// What each listing asks for in the fork word, most first. A kernel older
/// than an attribute refuses the whole request with `EINVAL` rather than
/// leaving it out, so a refused request is retried with less; an APFS
/// volume answers all of them, and others return none.
const SHARING_REQUESTS: [u32; 3] = [
    ATTR_CMNEXT_CLONEID | ATTR_CMNEXT_EXT_FLAGS | ATTR_CMNEXT_CLONE_REFCNT,
    ATTR_CMNEXT_EXT_FLAGS,
    0,
];
/// The first of [`SHARING_REQUESTS`] this kernel accepted, so only the
/// first listing pays for finding out.
static SHARING_LEVEL: AtomicUsize = AtomicUsize::new(0);
/// Set in a clone family's number to keep it apart from inode numbers,
/// which never reach it: the first file of a family shares its number with
/// its own inode, and a file edited away from the family keeps that inode.
const CLONE_FAMILY: u64 = 1 << 63;

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

    fn getattrlistat(
        dir: c_int,
        path: *const std::ffi::c_char,
        attr_list: *mut AttrList,
        attr_buf: *mut c_void,
        attr_buf_size: usize,
        options: u64,
    ) -> c_int;

    fn getattrlist(
        path: *const std::ffi::c_char,
        attr_list: *mut AttrList,
        attr_buf: *mut c_void,
        attr_buf_size: usize,
        options: u32,
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
    sharing: Sharing,
}

/// What a file shares with others, where the file system says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Sharing {
    own: Own,
    /// [`ATTR_CMNEXT_CLONEID`], only when the family has another member.
    family: Option<u64>,
}

/// How much of a file's allocation is its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Own {
    /// All of it: it shares no blocks, or the file system does not say.
    #[default]
    All,
    /// This many bytes; it shares the rest.
    Bytes(u64),
    /// Some of it: only [`ATTR_CMNEXT_PRIVATESIZE`] can say how much.
    Unknown,
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

    /// Whether the walk could meet this file's blocks again: under
    /// another name, or in a clone.
    pub const fn shared(&self) -> bool {
        self.links > 1 || self.sharing.family.is_some()
    }

    /// The identity a total charges once: the clone family's when the file
    /// has clones, since every member reports the family's blocks, and
    /// otherwise [`Self::identity`].
    pub const fn charged_as(&self) -> (u64, u64) {
        match self.sharing.family {
            Some(family) => (self.device, CLONE_FAMILY | family),
            None => self.identity(),
        }
    }

    /// Whether only [`ATTR_CMNEXT_PRIVATESIZE`] can say how much of the file
    /// is its own, and the answer is worth what asking costs.
    fn needs_private_size(&self) -> bool {
        self.sharing.own == Own::Unknown && self.allocated > PRIVATE_SIZE_FLOOR
    }

    /// The part of `size` no other file shares, so deleting
    /// the file is sure to free it; all of `size` where the file system
    /// does not say.
    pub fn unshared(&self, size: u64) -> u64 {
        match self.sharing.own {
            Own::All => size,
            Own::Bytes(own) => own.min(size),
            // Shares some blocks, and nobody could say how many are its
            // own: claim none rather than claim what may not come back.
            Own::Unknown => 0,
        }
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
            sharing: Sharing::default(),
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
    /// The [`SHARING_REQUESTS`] entry this listing asks with.
    level: usize,
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
        level: SHARING_LEVEL.load(Ordering::Relaxed),
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
        let fd = self.fd.as_raw_fd();
        let buffer = self.buffer.as_mut().expect("held until drop");
        let common = COMMON_ATTRIBUTES;
        request(&mut self.level, &SHARING_LEVEL, common, |list, options| {
            // SAFETY: the descriptor is an open directory owned by `self`;
            // `list` is a valid attrlist for the call's duration; and the
            // pointer and length describe `buffer`, which is writable for
            // that many bytes and not otherwise borrowed during the call.
            unsafe {
                getattrlistbulk(
                    fd,
                    list,
                    buffer.as_mut_ptr().cast(),
                    BUFFER_BYTES,
                    options,
                )
            }
        })
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

    /// The entry at `self.offset` and the record's length. The length is
    /// checked against the buffer, and [`parse`] checks the rest.
    fn record(&self) -> io::Result<(io::Result<Entry>, usize)> {
        let bytes = self.bytes();
        let length = read_u32(bytes, self.offset)? as usize;
        let record = bytes
            .get(self.offset..self.offset + length)
            .ok_or_else(malformed)?;
        let entry = match parse(record)? {
            Parsed::Failed(error) => Err(error),
            Parsed::Partial(name) => self.stat(name),
            Parsed::Measured(mut entry) => {
                if entry.needs_private_size() {
                    let name =
                        std::ffi::CString::new(entry.file_name().as_bytes());
                    if let Ok(name) = name {
                        entry.sharing.own =
                            private_size(self.fd.as_raw_fd(), &name, false);
                    }
                }
                Ok(entry)
            }
        };
        Ok((entry, length))
    }

    /// An entry the file system described only in part, read the ordinary
    /// way, relative to the directory already open.
    fn stat(&self, name: &[u8]) -> io::Result<Entry> {
        let stat =
            rustix::fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW)?;
        Ok(Entry::from_stat(name, &stat))
    }
}

/// Make one attribute request with the most sharing attributes this kernel
/// takes, from `level` on, and leave `level` at the request that worked,
/// which `remembered` learns too. `call` makes the request and returns
/// what the system call did; the answer is how many records it wrote.
fn request(
    level: &mut usize,
    remembered: &AtomicUsize,
    common: u32,
    mut call: impl FnMut(&mut AttrList, u64) -> c_int,
) -> io::Result<u32> {
    loop {
        let sharing = SHARING_REQUESTS[*level];
        let mut attributes = AttrList {
            bitmapcount: ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: common,
            volattr: 0,
            dirattr: 0,
            fileattr: FILE_ATTRIBUTES,
            forkattr: sharing,
        };
        let options = if sharing == 0 {
            0
        } else {
            FSOPT_ATTR_CMN_EXTENDED
        };
        if let Ok(count) = u32::try_from(call(&mut attributes, options)) {
            // Remembered only once a smaller request has worked, so a
            // directory that refuses for its own reasons cannot cost every
            // later listing its sharing.
            remembered.fetch_max(*level, Ordering::Relaxed);
            return Ok(count);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc_einval())
            && *level + 1 < SHARING_REQUESTS.len()
        {
            *level += 1;
        } else if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// `path` measured as a listing measures an entry, following a link at
/// `path` itself: for a file reached through a followed link, which no
/// listing of its own directory measured, so a clone met that way is still
/// charged as its family.
pub fn stat(path: &Path) -> io::Result<Entry> {
    let path_c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // A name is at most 255 UTF-16 units, so 765 bytes of UTF-8; the rest
    // is a few dozen bytes of attributes.
    let mut buffer = [0_u64; 128];
    let mut level = SHARING_LEVEL.load(Ordering::Relaxed);
    // `ATTR_CMN_ERROR` is for bulk listings only; one path reports its
    // error as the call's.
    request(
        &mut level,
        &SHARING_LEVEL,
        COMMON_ATTRIBUTES & !ATTR_CMN_ERROR,
        |attributes, options| {
            // SAFETY: `path_c` is a NUL-terminated path; `attributes` is a
            // valid attrlist for the call's duration; and the pointer and
            // length describe `buffer`, writable for that many bytes and
            // not otherwise borrowed during the call. The options fit:
            // only the low bits are flags.
            unsafe {
                getattrlist(
                    path_c.as_ptr(),
                    attributes,
                    buffer.as_mut_ptr().cast(),
                    size_of_val(&buffer),
                    options as u32,
                )
            }
        },
    )?;
    // SAFETY: as in `ReadDir::bytes`: `u64`s read as their bytes.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            buffer.as_ptr().cast::<u8>(),
            size_of_val(&buffer),
        )
    };
    let length = read_u32(bytes, 0)? as usize;
    let record = bytes.get(..length).ok_or_else(malformed)?;
    match parse(record)? {
        Parsed::Failed(error) => Err(error),
        Parsed::Partial(name) => rustix::fs::stat(path)
            .map(|stat| Entry::from_stat(name, &stat))
            .map_err(Into::into),
        Parsed::Measured(mut entry) => {
            if entry.needs_private_size() {
                let cwd = rustix::fs::CWD.as_raw_fd();
                entry.sharing.own = private_size(cwd, &path_c, true);
            }
            Ok(entry)
        }
    }
}

/// [`ATTR_CMNEXT_PRIVATESIZE`] of `name` in the directory `dir`, for a file
/// that shares some of its blocks; following a link at `name` itself when
/// `follow` says. [`Own::Unknown`] when the file system will not say.
fn private_size(dir: c_int, name: &std::ffi::CStr, follow: bool) -> Own {
    let mut attributes = AttrList {
        bitmapcount: ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: ATTR_CMN_RETURNED_ATTRS,
        volattr: 0,
        dirattr: 0,
        fileattr: 0,
        forkattr: ATTR_CMNEXT_PRIVATESIZE,
    };
    // The length, the returned attributes and the size.
    let mut buffer = [0_u64; 5];
    let options =
        FSOPT_ATTR_CMN_EXTENDED | if follow { 0 } else { FSOPT_NOFOLLOW };
    loop {
        // SAFETY: `dir` is an open directory or `AT_FDCWD`, `name` is
        // NUL-terminated, `attributes` is a valid attrlist for the call's
        // duration, and the pointer and length describe `buffer`, writable
        // for that many bytes and not otherwise borrowed during the call.
        let status = unsafe {
            getattrlistat(
                dir,
                name.as_ptr(),
                &raw mut attributes,
                buffer.as_mut_ptr().cast(),
                size_of_val(&buffer),
                options,
            )
        };
        if status == 0 {
            break;
        }
        if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Own::Unknown;
        }
    }
    // SAFETY: as in `ReadDir::bytes`: `u64`s read as their bytes.
    let record = unsafe {
        std::slice::from_raw_parts(
            buffer.as_ptr().cast::<u8>(),
            size_of_val(&buffer),
        )
    };
    // The returned fork word follows the length and four other words, and
    // the size follows the returned attributes.
    let mut cursor = Cursor { record, at: 20 };
    match (cursor.u32(), cursor.u64()) {
        (Ok(fork), Ok(own)) if fork & ATTR_CMNEXT_PRIVATESIZE != 0 => {
            Own::Bytes(own)
        }
        _ => Own::Unknown,
    }
}

/// What one record says.
enum Parsed<'a> {
    /// The entry could not be read.
    Failed(io::Error),
    /// The file system left out something the walk needs: a directory, or
    /// a file it measured only in part. The name, to read it another way.
    Partial(&'a [u8]),
    Measured(Entry),
}

/// Read one record, every length and offset checked against it first.
///
/// Layout per getattrlist(2): a `u32` length, the returned
/// `attribute_set_t`, `ATTR_CMN_ERROR` when it is set, then every other
/// attribute in bit order, each packed at 4-byte alignment.
fn parse(record: &[u8]) -> io::Result<Parsed<'_>> {
    let mut cursor = Cursor { record, at: 4 };
    let common = cursor.u32()?;
    // The volume and directory words: nothing asked for there.
    cursor.at += 8;
    let file = cursor.u32()?;
    let fork = cursor.u32()?;
    let error = if common & ATTR_CMN_ERROR == 0 {
        0
    } else {
        cursor.u32()?
    };
    let name = cursor.name()?;
    if error != 0 {
        let error = io::Error::from_raw_os_error(error.cast_signed());
        return Ok(Parsed::Failed(error));
    }
    let entry = if common & MEASURED == MEASURED {
        cursor.entry(file, fork, name)?
    } else {
        None
    };
    Ok(entry.map_or(Parsed::Partial(name), Parsed::Measured))
}

/// `EINVAL`, which rustix spells for its own error type only.
const fn libc_einval() -> i32 {
    rustix::io::Errno::INVAL.raw_os_error()
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
    fn entry(
        &mut self,
        file: u32,
        fork: u32,
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
        // After the file attributes, in bit order, whichever came back.
        let clone = if fork & ATTR_CMNEXT_CLONEID == 0 {
            None
        } else {
            Some(self.u64()?)
        };
        let shares = if fork & ATTR_CMNEXT_EXT_FLAGS == 0 {
            0
        } else {
            self.u64()?
        };
        let members = if fork & ATTR_CMNEXT_CLONE_REFCNT == 0 {
            0
        } else {
            self.u32()?
        };
        // A file never cloned counts no members, and one whose clones are
        // gone counts one: either way there is nothing to charge once.
        let family = clone.filter(|_| members > 1);
        // A family's members have the same blocks, so none has any of its
        // own. A file edited away from its family keeps sharing the rest,
        // and only its private size says how much is its own.
        let own = if shares & EF_MAY_SHARE_BLOCKS == 0 {
            Own::All
        } else if family.is_some() || shares & EF_SHARES_ALL_BLOCKS != 0 {
            Own::Bytes(0)
        } else {
            Own::Unknown
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
            sharing: Sharing { own, family },
        }))
    }
}

/// What tests of clones need, here and in the scan's tests.
#[cfg(test)]
pub mod testing {
    use std::path::Path;

    /// Whether `dir` is on APFS, the only file system here with clones.
    pub fn on_apfs(dir: &Path) -> bool {
        rustix::fs::statfs(dir).is_ok_and(|info| {
            let name: Vec<u8> = info
                .f_fstypename
                .iter()
                .take_while(|&&byte| byte != 0)
                .map(|&byte| byte.cast_unsigned())
                .collect();
            name == b"apfs"
        })
    }

    /// `cp -c`, which clones with clonefile(2).
    pub fn clone_file(from: &Path, to: &Path) {
        let status = std::process::Command::new("/bin/cp")
            .arg("-c")
            .arg(from)
            .arg(to)
            .status()
            .expect("cp");
        assert!(status.success(), "cp -c {}", from.display());
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{clone_file, on_apfs};
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

    fn named<'a>(entries: &'a [Entry], name: &str) -> &'a Entry {
        entries
            .iter()
            .find(|entry| entry.name() == name)
            .unwrap_or_else(|| panic!("no entry {name}"))
    }

    #[test]
    fn a_clone_family_is_one_identity_and_holds_nothing_of_its_own() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        if !on_apfs(root) {
            eprintln!("skipped: {} is not on APFS", root.display());
            return;
        }
        fs::write(root.join("original"), vec![7_u8; 1 << 20]).expect("write");
        clone_file(&root.join("original"), &root.join("copy"));
        fs::write(root.join("plain"), vec![7_u8; 1 << 20]).expect("write");

        let entries = listed(root);
        let (original, copy) =
            (named(&entries, "original"), named(&entries, "copy"));
        assert_ne!(original.identity(), copy.identity(), "two files");
        assert_eq!(original.charged_as(), copy.charged_as(), "one family");
        assert!(original.shared() && copy.shared());
        for member in [original, copy] {
            assert!(member.allocated() >= 1 << 20, "each reports it all");
            assert_eq!(
                member.unshared(member.allocated()),
                0,
                "every block of {} is the family's",
                member.name()
            );
        }
        let plain = named(&entries, "plain");
        assert!(!plain.shared());
        assert_eq!(plain.charged_as(), plain.identity());
        assert_eq!(plain.unshared(plain.allocated()), plain.allocated());

        // A write gives the copy blocks of its own and a family of its own;
        // the original, alone in the old one, is charged as itself again.
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(root.join("copy"))
            .expect("open");
        std::io::Write::write_all(&mut file, &vec![9_u8; 64 << 10])
            .expect("write");
        file.sync_all().expect("sync");
        let entries = listed(root);
        let (original, copy) =
            (named(&entries, "original"), named(&entries, "copy"));
        assert_ne!(original.charged_as(), copy.charged_as());
        assert_eq!(original.charged_as(), original.identity());
        let own = copy.unshared(copy.allocated());
        assert!(own >= 64 << 10, "what was written is its own: {own}");
        assert!(own < copy.allocated(), "the rest is still shared: {own}");
    }

    #[test]
    fn a_path_reads_as_its_listing_reads_it() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        fs::write(root.join("original"), vec![7_u8; 1 << 20]).expect("write");
        fs::write(root.join("edited"), vec![7_u8; 1 << 20]).expect("write");
        if on_apfs(root) {
            clone_file(&root.join("original"), &root.join("copy"));
            // Shares most of its blocks with the file it was cloned from,
            // which only its private size measures.
            clone_file(&root.join("edited"), &root.join("source"));
            let mut file = fs::OpenOptions::new()
                .write(true)
                .open(root.join("edited"))
                .expect("open");
            std::io::Write::write_all(&mut file, &vec![9_u8; 64 << 10])
                .expect("write");
            file.sync_all().expect("sync");
        }
        symlink("original", root.join("to-original")).expect("symlink");
        symlink("edited", root.join("to-edited")).expect("symlink");

        let entries = listed(root);
        for (name, link) in
            [("original", "to-original"), ("edited", "to-edited")]
        {
            let listed = named(&entries, name);
            for path in [root.join(name), root.join(link)] {
                let stated = stat(&path).expect("stat");
                assert_eq!(stated.kind(), Kind::File, "{}", path.display());
                assert_eq!(stated.identity(), listed.identity());
                assert_eq!(stated.charged_as(), listed.charged_as());
                assert_eq!(stated.allocated(), listed.allocated());
                assert_eq!(
                    stated.unshared(stated.allocated()),
                    listed.unshared(listed.allocated()),
                    "{}",
                    path.display()
                );
            }
        }
        if on_apfs(root) {
            let edited = named(&entries, "edited");
            let own = edited.unshared(edited.allocated());
            assert!((64 << 10..edited.allocated()).contains(&own), "{own}");
        }
        let error = stat(&root.join("gone")).expect_err("missing");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    /// A kernel that knows only some of the sharing attributes refuses a
    /// request for the rest with `EINVAL`; the request is made again with
    /// less, and the level that worked is remembered. An attribute bit no
    /// kernel knows stands in for one this kernel does not.
    #[test]
    fn a_refused_sharing_request_is_made_again_with_less() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("file");
        fs::write(&path, b"data").expect("write");
        let path_c =
            std::ffi::CString::new(path.as_os_str().as_bytes()).expect("path");
        let mut buffer = [0_u64; 128];
        let mut call = |list: &mut AttrList, options: u64| {
            if list.forkattr & ATTR_CMNEXT_CLONE_REFCNT != 0 {
                list.forkattr |= 0x8000_0000;
            }
            // SAFETY: as in `stat`.
            unsafe {
                getattrlist(
                    path_c.as_ptr(),
                    list,
                    buffer.as_mut_ptr().cast(),
                    size_of_val(&buffer),
                    options as u32,
                )
            }
        };
        let remembered = AtomicUsize::new(0);
        let mut level = 0;
        let common = COMMON_ATTRIBUTES & !ATTR_CMN_ERROR;
        request(&mut level, &remembered, common, &mut call).expect("less");
        assert_eq!(level, 1, "the sharing flags alone");
        assert_eq!(remembered.load(Ordering::Relaxed), 1);

        // Any other failure is the answer, not a reason to ask for less.
        let missing = std::ffi::CString::new(
            temp.path().join("gone").as_os_str().as_bytes(),
        )
        .expect("path");
        let mut level = 0;
        let error =
            request(&mut level, &remembered, common, |list, options| {
                // SAFETY: as in `stat`.
                unsafe {
                    getattrlist(
                        missing.as_ptr(),
                        list,
                        buffer.as_mut_ptr().cast(),
                        size_of_val(&buffer),
                        options as u32,
                    )
                }
            })
            .expect_err("missing");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(level, 0);
    }

    #[test]
    fn a_missing_directory_is_an_error() {
        let temp = TempDir::new().expect("tempdir");
        let error = read_dir(&temp.path().join("gone")).expect_err("missing");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}

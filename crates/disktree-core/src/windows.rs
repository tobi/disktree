//! What a walk on Windows needs beyond the standard library.
//!
//! `std::fs::read_dir` lists a directory with `FindNextFileW`, whose records
//! carry neither the space a file occupies nor a file id. Asking for them
//! file by file costs an open per file: on a home directory of 1.3 million
//! files that made the walk seven times slower. `GetFileInformationByHandleEx`
//! with `FileIdExtdDirectoryInfo` returns both for a whole buffer of entries,
//! for what the listing costs anyway.
//!
//! The rest is small: the volume a path is on, its free space, and comparing
//! paths the way Windows does.

#![allow(
    unsafe_code,
    reason = "Win32 calls std does not wrap; each block says why it is sound"
)]

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem::offset_of;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
use std::path::{Component, Path, PathBuf, Prefix};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
    FILE_OVERWRITE_IF, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
    FileRenameInformation, FileRenameInformationEx, NtCreateFile,
    NtSetInformationFile,
};
use windows_sys::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_INVALID_FUNCTION, ERROR_INVALID_LEVEL,
    ERROR_INVALID_PARAMETER, ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED,
    GENERIC_ALL, GENERIC_WRITE, INVALID_HANDLE_VALUE, MAX_PATH,
    OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce,
    GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
    GetSecurityDescriptorOwner, INHERIT_ONLY_ACE, IsValidAcl, IsWellKnownSid,
    OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
    WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateDirectoryW, DELETE, FILE_APPEND_DATA, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL,
    FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_DELETE_CHILD, FILE_DISPOSITION_INFO,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_ID_EXTD_DIR_INFO,
    FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_DATA,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO,
    FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, FileDispositionInfo,
    FileIdExtdDirectoryInfo, FileIdType, FileStandardInfo, FindFirstVolumeW,
    FindNextVolumeW, FindVolumeClose, GetDiskFreeSpaceExW,
    GetFileInformationByHandleEx, GetVolumeInformationW, GetVolumePathNameW,
    GetVolumePathNamesForVolumeNameW, OpenFileById, READ_CONTROL, ReOpenFile,
    SYNCHRONIZE, SetFileInformationByHandle, WRITE_DAC, WRITE_OWNER,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

use crate::space::SpaceInfo;

/// Bytes the file system fills per call: some hundreds of entries.
const BUFFER_BYTES: usize = 64 * 1024;

/// Where a record's name starts; everything before it has a fixed size.
const NAME_AT: usize = offset_of!(FILE_ID_EXTD_DIR_INFO, FileName);

/// Reparse tags with this bit stand for another path: symbolic links,
/// junctions and mounted folders. These are exactly what the standard
/// library calls symlinks, so this listing agrees with `std::fs::FileType`.
const NAME_SURROGATE: u32 = 0x2000_0000;

/// A `FILETIME` counts 100 ns ticks from 1601; this many precede 1970.
/// A cloud files placeholder (`OneDrive` and the like) with nothing local:
/// opening or listing it makes the provider fetch it. `RECALL_ON_OPEN` marks
/// a directory whose entries are not local, `RECALL_ON_DATA_ACCESS` one whose
/// files are all online-only.
const EVICTED: u32 =
    FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;

const EPOCH_TICKS: i64 = 116_444_736_000_000_000;
const TICKS_PER_SECOND: i64 = 10_000_000;

/// What an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Directory,
    /// A symbolic link, a junction or a mounted folder.
    Link,
    File,
}

/// One directory entry, with what a measurement needs.
///
/// The name is decoded to text once, here, since that is what the tree
/// keeps: the listing hands the walk one allocation per entry, not three.
#[derive(Debug)]
pub struct Entry {
    name: Box<str>,
    /// The name as Windows spells it, only when `name` could not: an
    /// unpaired surrogate is shown as U+FFFD but must still open.
    exact: Option<Box<OsStr>>,
    kind: Kind,
    apparent: u64,
    allocated: u64,
    modified: i64,
    identity: Option<(u64, u64)>,
    attributes: u32,
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

    pub const fn kind(&self) -> Kind {
        self.kind
    }

    /// The length, in bytes.
    pub const fn apparent(&self) -> u64 {
        self.apparent
    }

    /// Bytes the volume has allocated to the file, in whole clusters: less
    /// than the length for a compressed or sparse file, and zero for one
    /// whose data is kept in its file record. Where the listing could not say
    /// (see [`read_dir`]), the length.
    pub const fn allocated(&self) -> u64 {
        self.allocated
    }

    /// Last write, in Unix seconds; `0` when earlier or unknown.
    pub const fn modified(&self) -> i64 {
        self.modified
    }

    /// Whether Explorer hides this: `FILE_ATTRIBUTE_HIDDEN`, as on
    /// `AppData` and `$Recycle.Bin`.
    pub const fn hidden(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_HIDDEN != 0
    }

    /// Whether this is a directory the cloud files provider holds and the
    /// disk does not: see [`EVICTED`].
    pub const fn evicted(&self) -> bool {
        matches!(self.kind, Kind::Directory) && self.attributes & EVICTED != 0
    }

    /// `(volume serial, file id)`: the same for two names of one file.
    pub const fn identity(&self) -> Option<(u64, u64)> {
        self.identity
    }

    fn from_std(entry: &fs::DirEntry) -> io::Result<Self> {
        let meta = entry.metadata()?;
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() {
            Kind::Link
        } else if file_type.is_dir() {
            Kind::Directory
        } else {
            Kind::File
        };
        let (name, exact) = match entry.file_name().into_string() {
            Ok(name) => (name.into_boxed_str(), None),
            Err(raw) => {
                (raw.to_string_lossy().into(), Some(raw.into_boxed_os_str()))
            }
        };
        Ok(Self {
            name,
            exact,
            kind,
            apparent: meta.len(),
            allocated: meta.len(),
            modified: unix_seconds(
                i64::try_from(meta.last_write_time()).unwrap_or(0),
            ),
            identity: None,
            attributes: meta.file_attributes(),
        })
    }
}

/// A directory being listed; like `std::fs::ReadDir`, it leaves out `.`
/// and `..`.
#[derive(Debug)]
pub struct ReadDir {
    source: Source,
}

#[derive(Debug)]
enum Source {
    Records(Records),
    /// A file system or network share that does not implement the listing
    /// with ids: the standard one, which has lengths but not allocations.
    /// Boxed, being ten times the size of the common case.
    Std(Box<fs::ReadDir>),
}

#[derive(Debug)]
struct Records {
    handle: File,
    /// Serial number of the directory's volume, which with a file id names
    /// a file. `None` when it cannot be read; nothing is de-duplicated then.
    volume: Option<u64>,
    /// `u64`s for the eight-byte alignment the records are laid out in.
    buffer: Box<[u64]>,
    /// Where the next record starts, or `None` once the buffer is used up.
    next: Option<usize>,
    done: bool,
}

/// List `dir`, with each entry's allocation and file id.
///
/// `volume` is the serial number of the volume `dir` is on, when the
/// caller knows it for every directory of a walk (see [`walk_volume`]):
/// asking per directory is several requests to the file system on top of
/// the listing. `None` asks.
pub fn read_dir(dir: &Path, volume: Option<u64>) -> io::Result<ReadDir> {
    let handle = OpenOptions::new()
        .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
        // Needed to open a directory at all. Without
        // `FILE_FLAG_OPEN_REPARSE_POINT` a link is followed, which is what
        // a walk that chose to enter one wants.
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)?;
    let volume = volume.or_else(|| {
        winapi_util::file::information(&handle)
            .ok()
            .map(|info| info.volume_serial_number())
    });
    let mut records = Records {
        handle,
        volume,
        buffer: vec![0; BUFFER_BYTES / 8].into_boxed_slice(),
        next: None,
        done: false,
    };
    // The first fill shows whether the file system implements this listing,
    // before any entry has been handed out.
    let source = match records.fill() {
        Ok(()) => Source::Records(records),
        Err(error) if unsupported(&error) => {
            Source::Std(Box::new(fs::read_dir(dir)?))
        }
        Err(error) => return Err(error),
    };
    Ok(ReadDir { source })
}

impl ReadDir {
    /// Identity of the open directory, without opening its path a second
    /// time. A standard fallback listing has no handle or reliable ids.
    pub fn identity(&self) -> Option<(u64, u64)> {
        let Source::Records(records) = &self.source else {
            return None;
        };
        let info = winapi_util::file::information(&records.handle).ok()?;
        let id = info.file_index();
        (id != 0 && id != u64::MAX).then(|| (info.volume_serial_number(), id))
    }
}

impl Iterator for ReadDir {
    type Item = io::Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        let records = match &mut self.source {
            Source::Std(listing) => {
                return listing.next().map(|entry| {
                    entry.and_then(|entry| Entry::from_std(&entry))
                });
            }
            Source::Records(records) => records,
        };
        loop {
            if records.done {
                return None;
            }
            let Some(offset) = records.next else {
                if let Err(error) = records.fill() {
                    records.done = true;
                    return Some(Err(error));
                }
                continue;
            };
            match records.record(offset) {
                Ok((entry, next)) => {
                    records.next = next;
                    if let Some(entry) = entry {
                        return Some(Ok(entry));
                    }
                }
                Err(error) => {
                    records.done = true;
                    return Some(Err(error));
                }
            }
        }
    }
}

impl Records {
    /// Ask the file system for the next buffer of records.
    fn fill(&mut self) -> io::Result<()> {
        // SAFETY: the handle is an open directory handle owned by `self`;
        // the pointer and length describe `self.buffer`, which is writable
        // for that many bytes and not otherwise borrowed during the call;
        // and the class is the one the records are read as.
        let filled = unsafe {
            GetFileInformationByHandleEx(
                self.handle.as_raw_handle(),
                FileIdExtdDirectoryInfo,
                self.buffer.as_mut_ptr().cast(),
                BUFFER_BYTES as u32,
            )
        };
        if filled != 0 {
            self.next = Some(0);
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if code(&error) == Some(ERROR_NO_MORE_FILES) {
            self.done = true;
            return Ok(());
        }
        Err(error)
    }

    /// The buffer as the bytes the file system wrote.
    fn bytes(&self) -> &[u8] {
        // SAFETY: `[u64]` can be read as `u8`s of eight times its length:
        // one allocation, no alignment requirement for `u8`, and every byte
        // initialized, since the buffer starts zeroed.
        unsafe {
            std::slice::from_raw_parts(
                self.buffer.as_ptr().cast::<u8>(),
                size_of_val(&*self.buffer),
            )
        }
    }

    /// The entry at `offset`, or `None` for `.` and `..`, and where the next
    /// record starts. Every length the file system wrote is checked against
    /// the buffer before it is used.
    fn record(
        &self,
        offset: usize,
    ) -> io::Result<(Option<Entry>, Option<usize>)> {
        let malformed =
            || io::Error::other("the file system returned a malformed record");
        let bytes = self.bytes();
        let header =
            bytes.get(offset..offset + NAME_AT).ok_or_else(malformed)?;
        let field = |at: usize| {
            u32::from_ne_bytes(header[at..at + 4].try_into().expect("four"))
        };
        let signed = |at: usize| {
            i64::from_ne_bytes(header[at..at + 8].try_into().expect("eight"))
        };
        let name_len =
            field(offset_of!(FILE_ID_EXTD_DIR_INFO, FileNameLength)) as usize;
        let name_bytes = bytes
            .get(offset + NAME_AT..offset + NAME_AT + name_len)
            .filter(|name| name.len() % 2 == 0)
            .ok_or_else(malformed)?;
        let next =
            match field(offset_of!(FILE_ID_EXTD_DIR_INFO, NextEntryOffset)) {
                0 => None,
                step if step as usize >= NAME_AT + name_len => {
                    Some(offset + step as usize)
                }
                _ => return Err(malformed()),
            };

        let units = name_bytes
            .chunks_exact(2)
            .map(|pair| u16::from_ne_bytes([pair[0], pair[1]]));
        let (name, exact) = decode_name(units);
        if &*name == "." || &*name == ".." {
            return Ok((None, next));
        }

        let attributes =
            field(offset_of!(FILE_ID_EXTD_DIR_INFO, FileAttributes));
        let tag = field(offset_of!(FILE_ID_EXTD_DIR_INFO, ReparsePointTag));
        let kind = if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            && tag & NAME_SURROGATE != 0
        {
            Kind::Link
        } else if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            Kind::Directory
        } else {
            Kind::File
        };
        let id_at = offset_of!(FILE_ID_EXTD_DIR_INFO, FileId);
        let id = u128::from_le_bytes(
            header[id_at..id_at + 16].try_into().expect("sixteen"),
        );
        let entry = Entry {
            name,
            exact,
            kind,
            apparent: bytes_from(signed(offset_of!(
                FILE_ID_EXTD_DIR_INFO,
                EndOfFile
            ))),
            allocated: bytes_from(signed(offset_of!(
                FILE_ID_EXTD_DIR_INFO,
                AllocationSize
            ))),
            modified: unix_seconds(signed(offset_of!(
                FILE_ID_EXTD_DIR_INFO,
                LastWriteTime
            ))),
            identity: self.volume.zip(file_id(id)),
            attributes,
        };
        Ok((Some(entry), next))
    }
}

/// A UTF-16 name as text, decoded in one pass; nearly every name is ASCII,
/// so one code unit is one byte and the text is allocated once, at its
/// final size. An unpaired surrogate is replaced, and the exact name kept
/// beside it so the path still opens.
fn decode_name(
    units: impl Iterator<Item = u16> + Clone,
) -> (Box<str>, Option<Box<OsStr>>) {
    let mut name = String::with_capacity(units.size_hint().0);
    let mut lossy = false;
    for unit in char::decode_utf16(units.clone()) {
        name.push(unit.unwrap_or_else(|_| {
            lossy = true;
            char::REPLACEMENT_CHARACTER
        }));
    }
    let exact = lossy.then(|| {
        OsString::from_wide(&units.collect::<Vec<u16>>()).into_boxed_os_str()
    });
    (name.into_boxed_str(), exact)
}

/// The part of a 128-bit file id that names a file by itself.
///
/// NTFS keeps the high half zero ([MS-FSCC] 2.1.10), so the low half is the
/// id. Zero is what a file system without ids reports; FAT and exFAT do,
/// and their 64-bit ids are documented as not unique, so trusting them could
/// merge two files into one.
///
/// [MS-FSCC]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/98860416-1caf-4c80-a9ab-8d61e1ccf5a5
fn file_id(id: u128) -> Option<u64> {
    // TODO(refs-hardlinks): ReFS ids use both halves, which a (u64, u64)
    // key cannot hold together with the volume, so hardlinks on ReFS (Dev
    // Drives) are counted once per name. That overcounts; it never drops a
    // file's size.
    u64::try_from(id).ok().filter(|&low| low != 0)
}

/// `(volume serial, file index)` of what `path` names, following links: how
/// a walk that follows links notices it has been somewhere before. `None`
/// where there is no usable index; zero and all ones mean exactly that
/// ([MS-FSCC] 2.1.9).
///
/// [MS-FSCC]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-fscc/2d3333fe-fc98-4a6f-98a2-4bb805aff407
pub fn identity(path: &Path) -> Option<(u64, u64)> {
    let info = information(path)?;
    let index = info.file_index();
    (index != 0 && index != u64::MAX)
        .then(|| (info.volume_serial_number(), index))
}

/// Allocation (or length) and Unix write time from a known file's stream.
/// Directory entries can retain different allocations for hardlink names.
/// The identity must match; reparse points are not followed. The open asks
/// only for attributes, shares read/write/delete, and reads no file data.
pub fn current_file(
    path: &Path,
    expected: (u64, u64),
    apparent: bool,
) -> Option<(u64, i64)> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .ok()?;
    let info = winapi_util::file::information(&file).ok()?;
    if (info.volume_serial_number(), info.file_index()) != expected
        || info.file_attributes() & u64::from(FILE_ATTRIBUTE_DIRECTORY) != 0
    {
        return None;
    }
    let (length, allocated) = standard_sizes(&file).ok()?;
    let bytes = if apparent { length } else { allocated };
    let modified = info
        .last_write_time()
        .and_then(|ticks| i64::try_from(ticks).ok())
        .map_or(0, unix_seconds);
    Some((bytes, modified))
}

/// Serial number of the volume every directory under `canonical` is on,
/// for a walk that does not follow links. Only a local drive has one:
/// mounted folders there are reparse points the walk takes for links,
/// while a share's DFS links are not, and lead to other servers' volumes.
/// `None` for a share, or when the serial cannot be read.
pub fn walk_volume(canonical: &Path) -> Option<u64> {
    let Some(Component::Prefix(prefix)) = canonical.components().next() else {
        return None;
    };
    if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) {
        return None;
    }
    information(canonical).map(|info| info.volume_serial_number())
}

/// What the file system says about `path`, following links.
fn information(path: &Path) -> Option<winapi_util::file::Information> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
        .ok()?;
    winapi_util::file::information(&file).ok()
}

/// Capacity and free space of the volume holding `path`.
pub fn space_info(path: &Path) -> io::Result<SpaceInfo> {
    // A network share is only accepted with a trailing separator.
    let directory = wide(path, true)?;
    let (mut available, mut total, mut free) = (0_u64, 0_u64, 0_u64);
    // SAFETY: `directory` is NUL-terminated and outlives the call, and each
    // out pointer is to a live, writable `u64`.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            directory.as_ptr(),
            &raw mut available,
            &raw mut total,
            &raw mut free,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(SpaceInfo {
        total,
        free,
        available,
    })
}

/// Where the volume holding `path` is mounted: a drive such as `C:\`, a
/// share, or a folder another volume is mounted on.
pub fn volume_root(path: &Path) -> Option<PathBuf> {
    let name = wide(path, false).ok()?;
    // The answer is a prefix of the path made absolute; the path's length
    // plus room for the longest drive and directory it could gain is ample.
    let mut root = vec![0_u16; name.len() + 260];
    // SAFETY: `name` is NUL-terminated and outlives the call, and `root` is
    // writable for the length passed.
    let ok = unsafe {
        GetVolumePathNameW(
            name.as_ptr(),
            root.as_mut_ptr(),
            u32::try_from(root.len()).ok()?,
        )
    };
    if ok == 0 {
        return None;
    }
    let end = root.iter().position(|&unit| unit == 0)?;
    Some(PathBuf::from(OsString::from_wide(&root[..end])))
}

/// Every place a volume is mounted: drive roots such as `D:\`, and folders
/// a volume is mounted on, such as `C:\Data\Disk2\`. The mount table
/// Windows has in place of `/proc/self/mounts`.
pub fn mount_points() -> Vec<PathBuf> {
    let mut points = Vec::new();
    // A volume GUID path, `\\?\Volume{…}\`, is 49 units with its NUL.
    let mut volume = [0_u16; 64];
    let length = u32::try_from(volume.len()).unwrap_or(u32::MAX);
    // SAFETY: `volume` is writable for the length passed.
    let find = unsafe { FindFirstVolumeW(volume.as_mut_ptr(), length) };
    if find.is_null() || find == INVALID_HANDLE_VALUE {
        return points;
    }
    loop {
        points.extend(volume_paths(&volume));
        // SAFETY: `find` is a live search handle, and `volume` is writable
        // for the length passed.
        if unsafe { FindNextVolumeW(find, volume.as_mut_ptr(), length) } == 0 {
            break;
        }
    }
    // SAFETY: `find` is a live search handle, closed once.
    unsafe { FindVolumeClose(find) };
    points
}

/// The paths the volume named by the NUL-terminated `volume` is mounted at.
fn volume_paths(volume: &[u16]) -> Vec<PathBuf> {
    let mut names = vec![0_u16; 1024];
    loop {
        let mut needed = 0_u32;
        let Ok(length) = u32::try_from(names.len()) else {
            return Vec::new();
        };
        // SAFETY: `volume` is NUL-terminated, `names` is writable for the
        // length passed, and `needed` is a live `u32`.
        let ok = unsafe {
            GetVolumePathNamesForVolumeNameW(
                volume.as_ptr(),
                names.as_mut_ptr(),
                length,
                &raw mut needed,
            )
        };
        if ok != 0 {
            break;
        }
        // Too small: grow to what it asked for, once more at most.
        let needed = usize::try_from(needed).unwrap_or(0);
        if needed <= names.len() {
            return Vec::new();
        }
        names.resize(needed, 0);
    }
    // NUL-separated names, ended by an empty one.
    names
        .split(|&unit| unit == 0)
        .take_while(|name| !name.is_empty())
        .map(|name| PathBuf::from(OsString::from_wide(name)))
        .collect()
}

/// Whether `path` is a folder another volume is mounted on. Removing one
/// would reach onto that volume, which nobody marked.
pub fn is_mount_point(path: &Path) -> bool {
    path.parent().is_some()
        && volume_root(path).is_some_and(|root| same(&root, path))
}

/// Whether this process runs as an administrator, with the elevated token
/// UAC hands out: what reading a volume directly takes.
pub fn elevated() -> bool {
    // SAFETY: takes no arguments and only reads the process token.
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

/// Whether `root` is a whole drive an administrator would read from its
/// file table: a drive letter's root, formatted NTFS. Anything else, from
/// a folder or a mounted folder to another file system, is walked either way.
pub fn file_table_readable(root: &Path) -> bool {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    drive_letter(&canonical)
        .is_some_and(|letter| ntfs(Path::new(&format!("{letter}:\\"))))
}

/// Whether `path` is on an NTFS volume, where a file id is the file's
/// record with the record's reuse count in its top 16 bits.
pub fn on_ntfs(path: &Path) -> bool {
    volume_root(path).is_some_and(|root| ntfs(&root))
}

/// Whether the volume mounted at `root`, such as `C:\`, is formatted NTFS.
fn ntfs(root: &Path) -> bool {
    let Ok(volume) = wide(root, true) else {
        return false;
    };
    let mut name = [0_u16; MAX_PATH as usize + 1];
    let length = u32::try_from(name.len()).unwrap_or(u32::MAX);
    // SAFETY: `volume` is NUL-terminated and outlives the call, `name` is
    // writable for the length passed, and the other outputs may be null.
    let ok = unsafe {
        GetVolumeInformationW(
            volume.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            length,
        )
    };
    let end = name.iter().position(|&unit| unit == 0).unwrap_or(0);
    ok != 0 && String::from_utf16_lossy(&name[..end]) == "NTFS"
}

/// Start `program` with `args` as an administrator, through the UAC
/// prompt. Returns once the prompt is answered; declining it is an error
/// (`ERROR_CANCELLED`).
pub fn run_elevated(program: &Path, args: &[OsString]) -> io::Result<()> {
    let file = wide(program, false)?;
    let mut line: Vec<u16> = Vec::new();
    for arg in args {
        if !line.is_empty() {
            line.push(u16::from(b' '));
        }
        line.extend(quoted(arg));
    }
    line.push(0);
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    // SAFETY: every string is NUL-terminated and outlives the call; a null
    // window and directory are allowed.
    let instance = unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            line.as_ptr(),
            std::ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    // Anything above 32 is success, by the function's own convention.
    if instance as usize > 32 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Write out what the file system holds only in memory for the volume
/// holding `path`, its file table included. Flushing a volume takes write
/// access, so an administrator. Only a volume mounted as a drive letter:
/// for one mounted in a folder, `\\.\X:` would name the drive around it.
pub fn flush_volume(path: &Path) -> io::Result<()> {
    let root = volume_root(path).ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "no volume holds the path")
    })?;
    let letter = drive_letter(&root)
        .ok_or_else(|| io::Error::from(io::ErrorKind::Unsupported))?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(format!(r"\\.\{letter}:"))?
        .sync_all()
}

/// `C` when `canonical` is the root of drive `C:`; `None` for anything
/// else, a folder or a volume mounted in one.
pub fn drive_letter(canonical: &Path) -> Option<char> {
    let mut components = canonical.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return None;
    };
    let (Prefix::Disk(letter) | Prefix::VerbatimDisk(letter)) = prefix.kind()
    else {
        return None;
    };
    (components.next() == Some(Component::RootDir)
        && components.next().is_none())
    .then_some(char::from(letter))
}

/// The folder that holds every user's profile, `C:\Users` as installed,
/// from Windows itself: it can be moved, so no path is assumed. `None`
/// when Windows does not say.
pub fn user_profiles_dir() -> Option<PathBuf> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_UserProfiles, SHGetKnownFolderPath,
    };
    let mut raw: *mut u16 = std::ptr::null_mut();
    // SAFETY: the id outlives the call, a null token means this user, and
    // `raw` receives a string the shell allocates, freed below either way.
    let result = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_UserProfiles,
            0,
            std::ptr::null_mut(),
            &raw mut raw,
        )
    };
    let path = (result >= 0 && !raw.is_null()).then(|| {
        // SAFETY: on success `raw` is a NUL-terminated string, read up to
        // and not past its NUL.
        unsafe {
            let mut len = 0;
            while *raw.add(len) != 0 {
                len += 1;
            }
            PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(
                raw, len,
            )))
        }
    });
    // SAFETY: `raw` came from the shell's allocator, or is null, which
    // the call accepts.
    unsafe { CoTaskMemFree(raw.cast()) };
    path
}

/// `arg` quoted so a program's C runtime splits it back out whole: inside
/// quotes, backslashes are literal except before a quote, where they must
/// be doubled, so `C:\` becomes `"C:\\"` and not `"C:\"`.
fn quoted(arg: &OsStr) -> Vec<u16> {
    let quote = u16::from(b'"');
    let backslash = u16::from(b'\\');
    let mut out = vec![quote];
    let mut slashes = 0;
    for unit in arg.encode_wide() {
        if unit == backslash {
            slashes += 1;
            continue;
        }
        let run = if unit == quote {
            2 * slashes + 1
        } else {
            slashes
        };
        out.extend(std::iter::repeat_n(backslash, run));
        slashes = 0;
        out.push(unit);
    }
    out.extend(std::iter::repeat_n(backslash, 2 * slashes));
    out.push(quote);
    out
}

/// Whether `left` and `right` name the same place, compared the way Windows
/// compares names: see [`folded`].
pub fn same(left: &Path, right: &Path) -> bool {
    folded(left) == folded(right)
}

/// `path` in the one spelling the removal guards compare: [`folded`], put
/// back together, so `Path::starts_with` on two keys is containment as
/// Windows sees it.
pub fn guard_key(path: &Path) -> PathBuf {
    folded(path).into_iter().collect()
}

/// Each component in the form Windows would treat as equal: without regard
/// to case, so `C:\WINDOWS\System32` is inside `C:\Windows`, and with
/// `\\?\C:\` the same place as `C:\`.
fn folded(path: &Path) -> Vec<String> {
    path.components()
        .map(|component| match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                    format!("{}:", char::from(letter.to_ascii_lowercase()))
                }
                Prefix::UNC(server, share)
                | Prefix::VerbatimUNC(server, share) => format!(
                    r"\\{}\{}",
                    server.to_string_lossy(),
                    share.to_string_lossy()
                )
                .to_lowercase(),
                Prefix::Verbatim(_) | Prefix::DeviceNS(_) => {
                    prefix.as_os_str().to_string_lossy().to_lowercase()
                }
            },
            other => other.as_os_str().to_string_lossy().to_lowercase(),
        })
        .collect()
}

/// `path` as a NUL-terminated wide string, with a trailing separator when
/// `directory` is set.
fn wide(path: &Path, directory: bool) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a path cannot contain NUL",
        ));
    }
    let separator =
        |unit: &u16| *unit == u16::from(b'\\') || *unit == u16::from(b'/');
    if directory && !wide.last().is_some_and(separator) {
        wide.push(u16::from(b'\\'));
    }
    wide.push(0);
    Ok(wide)
}

/// Errors a file system or redirector gives for a listing it does not
/// implement.
fn unsupported(error: &io::Error) -> bool {
    matches!(
        code(error),
        Some(
            ERROR_INVALID_FUNCTION
                | ERROR_NOT_SUPPORTED
                | ERROR_INVALID_PARAMETER
                | ERROR_INVALID_LEVEL
        )
    )
}

fn code(error: &io::Error) -> Option<u32> {
    error
        .raw_os_error()
        .and_then(|code| u32::try_from(code).ok())
}

/// A byte count the file system reports as signed; it never is negative.
fn bytes_from(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// A `FILETIME` as Unix seconds, `0` before 1970.
pub const fn unix_seconds(ticks: i64) -> i64 {
    if ticks <= EPOCH_TICKS {
        0
    } else {
        (ticks - EPOCH_TICKS) / TICKS_PER_SECOND
    }
}

/// Send control `code` to the device or file behind `handle`, with `input`,
/// filling `output`: the bytes it wrote.
pub fn control(
    handle: &File,
    code: u32,
    input: &[u8],
    output: &mut [u8],
) -> io::Result<usize> {
    let mut returned = 0_u32;
    let (Ok(in_len), Ok(out_len)) =
        (u32::try_from(input.len()), u32::try_from(output.len()))
    else {
        return Err(io::ErrorKind::InvalidInput.into());
    };
    // SAFETY: both buffers are live for the lengths passed, `input` is only
    // read and `output` only written, and without an `OVERLAPPED` the call
    // is done with them when it returns.
    let ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle.as_raw_handle(),
            code,
            input.as_ptr().cast(),
            in_len,
            output.as_mut_ptr().cast(),
            out_len,
            &raw mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(returned as usize)
}

/// A file's length and the bytes allocated to its unnamed stream as NTFS
/// has them now, the file found by its reference (record number and
/// sequence number) on `volume`'s volume. Opened for its attributes only,
/// which no sharing mode refuses, and by number, so no path is needed.
pub fn sizes_by_id(volume: &File, reference: u64) -> io::Result<(u64, u64)> {
    let descriptor = FILE_ID_DESCRIPTOR {
        dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
        Type: FileIdType,
        Anonymous: FILE_ID_DESCRIPTOR_0 {
            FileId: reference.cast_signed(),
        },
    };
    // SAFETY: the descriptor is live for the call, and no security
    // attributes are passed.
    let handle = unsafe {
        OpenFileById(
            volume.as_raw_handle(),
            &raw const descriptor,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a handle just opened and owned by nothing else; `File`
    // closes it.
    let file = unsafe { File::from_raw_handle(handle) };
    standard_sizes(&file)
}

fn standard_sizes(file: &File) -> io::Result<(u64, u64)> {
    const SIZE: usize = size_of::<FILE_STANDARD_INFO>();
    // The trailing flags in FILE_STANDARD_INFO are Rust bools; receive
    // kernel bytes into integers instead, so no invalid bool is made.
    let mut info = [0_u64; SIZE.div_ceil(8)];
    // SAFETY: the live file handle fills a writable, aligned buffer of
    // at least SIZE bytes. No other reference uses it during the call.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            info.as_mut_ptr().cast(),
            SIZE as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        bytes_from(
            info[offset_of!(FILE_STANDARD_INFO, EndOfFile) / 8].cast_signed(),
        ),
        bytes_from(
            info[offset_of!(FILE_STANDARD_INFO, AllocationSize) / 8]
                .cast_signed(),
        ),
    ))
}

/// What an elevated process makes its caches with: owned by the
/// Administrators group, under a protected ACL that grants Administrators
/// and SYSTEM alone any access. No ordinary process of the user can then
/// change what an elevated scan reads back and trusts.
const CACHE_SDDL: &str = "O:BAG:BAD:P(A;;FA;;;BA)(A;;FA;;;SY)";

/// Rights that change a file's bytes or attributes, or who may: granted to
/// anyone but Administrators or SYSTEM, a cache is not trusted. On a
/// directory the same bits add and delete its entries.
const CACHE_WRITE_RIGHTS: u32 = GENERIC_ALL
    | GENERIC_WRITE
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_DELETE_CHILD
    | FILE_WRITE_ATTRIBUTES;

/// `ACCESS_ALLOWED_ACE_TYPE` and `ACCESS_DENIED_ACE_TYPE`.
const ALLOWED_ACE: u8 = 0;
const DENIED_ACE: u8 = 1;

/// `FILE_RENAME_FLAG_REPLACE_IF_EXISTS` and `_POSIX_SEMANTICS`.
const RENAME_REPLACE: u32 = 1;
const RENAME_POSIX: u32 = 2;

/// A security descriptor Win32 allocated, freed when dropped.
struct CacheSecurity(*mut core::ffi::c_void);

impl Drop for CacheSecurity {
    fn drop(&mut self) {
        // SAFETY: Win32 allocated this descriptor with LocalAlloc; this
        // wrapper alone owns it and drops it once after all calls finish.
        unsafe { windows_sys::Win32::Foundation::LocalFree(self.0) };
    }
}

fn cache_security() -> io::Result<CacheSecurity> {
    descriptor_from_sddl(CACHE_SDDL)
}

fn descriptor_from_sddl(sddl: &str) -> io::Result<CacheSecurity> {
    let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: the NUL-terminated SDDL lives throughout the call; the
    // output pointer receives a LocalAlloc descriptor owned by the caller.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide.as_ptr(),
            1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(CacheSecurity(descriptor))
}

/// Whether `sid` is the Administrators group or SYSTEM.
fn admin_sid(sid: *mut core::ffi::c_void) -> bool {
    // SAFETY: callers pass a SID returned by Win32 or one whose complete
    // byte range was checked inside a validated ACL.
    unsafe {
        !sid.is_null()
            && (IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
                || IsWellKnownSid(sid, WinLocalSystemSid) != 0)
    }
}

/// Whether `descriptor` holds only what Administrators and SYSTEM can
/// change: owned by one of them, under a protected ACL, which inheritance
/// cannot widen later, that grants no one else a right to write.
fn trusted_descriptor(descriptor: *mut core::ffi::c_void) -> bool {
    let mut owner = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut present = 0;
    let mut control = 0;
    let mut defaulted = 0;
    let mut revision = 0;
    // SAFETY: descriptor is a live security descriptor from Win32. Each
    // output is a live local, and the returned pointers live with it.
    let valid = unsafe {
        GetSecurityDescriptorOwner(
            descriptor,
            &raw mut owner,
            &raw mut defaulted,
        ) != 0
            && GetSecurityDescriptorDacl(
                descriptor,
                &raw mut present,
                &raw mut dacl,
                &raw mut defaulted,
            ) != 0
            && GetSecurityDescriptorControl(
                descriptor,
                &raw mut control,
                &raw mut revision,
            ) != 0
    };
    if !valid
        || !admin_sid(owner)
        || present == 0
        || dacl.is_null()
        || control & SE_DACL_PROTECTED == 0
    {
        return false;
    }
    // SAFETY: dacl points into the live descriptor; IsValidAcl verifies
    // its size and ACE boundaries before GetAce is asked for any entry.
    if unsafe { IsValidAcl(dacl) } == 0 {
        return false;
    }
    // SAFETY: dacl is valid and still belongs to the live descriptor.
    let count = unsafe { (*dacl).AceCount };
    for index in 0..u32::from(count) {
        let mut ace = std::ptr::null_mut();
        // SAFETY: a validated ACL has count ACEs; GetAce checks the index
        // and gives a pointer within the descriptor.
        if unsafe { GetAce(dacl, index, &raw mut ace) } == 0 {
            return false;
        }
        // SAFETY: GetAce gave a pointer to an ACE_HEADER within the ACL.
        let header = unsafe { ace.cast::<ACE_HEADER>().read_unaligned() };
        // An inherit-only entry applies to what is made inside later, not
        // to this; a denial only takes rights away.
        if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0
            || header.AceType == DENIED_ACE
        {
            continue;
        }
        // Object, callback and other entries that allow: not understood
        // here, so not trusted.
        if header.AceType != ALLOWED_ACE {
            return false;
        }
        // The mask, then the SID: revision, count and authority, then the
        // count's sub-authorities, all within the entry.
        let sid_at = offset_of!(ACCESS_ALLOWED_ACE, SidStart);
        let size = usize::from(header.AceSize);
        if size < sid_at + 8 {
            return false;
        }
        let ace = ace.cast::<u8>();
        // SAFETY: IsValidAcl checked that the entry's `size` bytes lie in
        // the ACL, and the mask and the SID's first 8 bytes are within them.
        let (mask, subs, sid) = unsafe {
            (
                ace.add(offset_of!(ACCESS_ALLOWED_ACE, Mask))
                    .cast::<u32>()
                    .read_unaligned(),
                usize::from(*ace.add(sid_at + 1)),
                ace.add(sid_at),
            )
        };
        if mask & CACHE_WRITE_RIGHTS != 0
            && (sid_at + 8 + 4 * subs > size || !admin_sid(sid.cast()))
        {
            return false;
        }
    }
    true
}

fn trusted_handle(file: &File) -> bool {
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: the handle is open with READ_CONTROL and the out parameter
    // receives a LocalAlloc descriptor. The other outputs are unused.
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if result != 0 || descriptor.is_null() {
        return false;
    }
    let owned = CacheSecurity(descriptor);
    trusted_descriptor(owned.0)
}

fn untrusted() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, "untrusted cache")
}

/// Where the cache `name` under `dir` is kept: `mft-<drive>.bin` for a
/// file table read, `walk-<root>.bin` for a walk. An elevated process
/// keeps its own apart, in `admin`, and never reads the ones the user's
/// ordinary processes keep beside it.
pub fn cache_path(dir: &Path, name: &str) -> PathBuf {
    if elevated() {
        dir.join("admin").join(name)
    } else {
        dir.join(name)
    }
}

/// The directory `dir`, open, and for `create` made first. An elevated
/// process makes it with the admin-only owner and ACL, and takes it only
/// while it is still that, and a directory rather than a link to one. It
/// holds it without share-delete, so no one moves it away meanwhile.
fn cache_directory(dir: &Path, create: bool, admin: bool) -> io::Result<File> {
    if !admin {
        if create {
            fs::create_dir_all(dir)?;
        }
        // The user's own: a folder moved elsewhere and linked back is
        // followed, as any other path of theirs.
        return OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(dir);
    }
    if create {
        let parent = dir
            .parent()
            .ok_or_else(|| io::Error::other("cache parent missing"))?;
        fs::create_dir_all(parent)?;
        let descriptor = cache_security()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        let name = wide(dir, false)?;
        // SAFETY: the path is NUL-terminated; the attributes and the
        // descriptor they point at are live for the call.
        let made =
            unsafe { CreateDirectoryW(name.as_ptr(), &raw const attributes) };
        if made == 0 {
            // One already there is checked below, like any other.
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS.cast_signed())
            {
                return Err(error);
            }
        }
    }
    let handle = OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(dir)?;
    let attributes = winapi_util::file::information(&handle)?.file_attributes();
    if attributes & u64::from(FILE_ATTRIBUTE_DIRECTORY) == 0
        || attributes & u64::from(FILE_ATTRIBUTE_REPARSE_POINT) != 0
        || !trusted_handle(&handle)
    {
        return Err(untrusted());
    }
    Ok(handle)
}

/// Open, or as `disposition` says make, the file `name` in the open
/// `directory`, through its handle: nothing above it is looked up again. A
/// link named `name` is opened itself, not followed.
fn open_in(
    directory: &File,
    name: &OsStr,
    access: u32,
    share: u32,
    disposition: u32,
    security: Option<&CacheSecurity>,
) -> io::Result<File> {
    let name: Vec<u16> = name.encode_wide().collect();
    let length = u16::try_from(name.len() * 2).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "cache name too long")
    })?;
    let object = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: name.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: directory.as_raw_handle(),
        ObjectName: &raw const object,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: security
            .map_or(std::ptr::null(), |security| security.0.cast()),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut handle = std::ptr::null_mut();
    let mut status = IO_STATUS_BLOCK::default();
    // SAFETY: `attributes` and all it points at (the name, the directory's
    // open handle, the descriptor `security` owns) are live for the call;
    // the outputs are live locals; no extended attributes are passed.
    let result = unsafe {
        NtCreateFile(
            &raw mut handle,
            access | SYNCHRONIZE,
            &raw const attributes,
            &raw mut status,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            share,
            disposition,
            FILE_NON_DIRECTORY_FILE
                | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_OPEN_REPARSE_POINT,
            std::ptr::null(),
            0,
        )
    };
    if result < 0 {
        return Err(nt_error(result));
    }
    // SAFETY: a handle just made and owned by nothing else; `File` closes
    // it.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// The cache at `path`, open for reading; `None` sends the scan back to a
/// whole read. An elevated process reads only a file Administrators own
/// and alone can change, in a directory the same holds for, and checks
/// the very handle it then reads.
pub fn cache_read(path: &Path) -> Option<File> {
    read_cache(path, elevated())
}

fn read_cache(path: &Path, admin: bool) -> Option<File> {
    let directory = cache_directory(path.parent()?, false, admin).ok()?;
    let file = open_in(
        &directory,
        path.file_name()?,
        READ_CONTROL | FILE_READ_DATA | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_OPEN,
        None,
    )
    .ok()?;
    let attributes = winapi_util::file::information(&file)
        .ok()?
        .file_attributes();
    (attributes & u64::from(FILE_ATTRIBUTE_REPARSE_POINT) == 0
        && (!admin || trusted_handle(&file)))
    .then_some(file)
}

/// Another handle on the file `original` has open, for reads at offsets of
/// their own: one synchronous handle serializes its reads. Made from the
/// handle, not the path, so it is the file already checked.
pub fn cache_reopen(original: &File) -> io::Result<File> {
    // SAFETY: original is a live file handle; ReOpenFile returns an owned
    // handle to the same stream, not a path that could be swapped.
    let handle = unsafe {
        ReOpenFile(
            original.as_raw_handle(),
            FILE_READ_DATA | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_OPEN_REPARSE_POINT,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ReOpenFile returned a new handle owned by this File alone.
    Ok(unsafe { File::from_raw_handle(handle) })
}

/// Write the cache at `path` whole or not at all: `write` fills another
/// file beside it, which then takes its name; on any failure that file is
/// deleted and `path` keeps what it had. Both go through the directory's
/// handle, not its path, so a link planted above it cannot send either
/// elsewhere; an elevated process's file has the admin-only owner and ACL
/// from the moment it exists.
pub fn cache_write(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    write_cache(path, elevated(), write)
}

fn write_cache(
    path: &Path,
    admin: bool,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a cache is a file in a directory",
        ));
    };
    let directory = cache_directory(dir, true, admin)?;
    // One such name per cache: what a save cut short by a crash leaves is
    // overwritten by the next, not left beside it for good, and sharing
    // it with readers alone keeps a second save out while one writes. In
    // an elevated process's directory nobody else can have made it.
    let mut partial = name.to_os_string();
    partial.push(".partial");
    let security = if admin { Some(cache_security()?) } else { None };
    let mut file = open_in(
        &directory,
        &partial,
        DELETE | READ_CONTROL | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ,
        FILE_OVERWRITE_IF,
        security.as_ref(),
    )?;
    // A file system that keeps no owner or ACL leaves it untrusted, as
    // does one left with another ACL; deleted below, it is made anew next
    // time.
    let written = if admin && !trusted_handle(&file) {
        Err(untrusted())
    } else {
        write(&mut file).and_then(|()| rename_in(&directory, &file, name))
    };
    if written.is_err() {
        let gone = FILE_DISPOSITION_INFO { DeleteFile: true };
        // SAFETY: the handle is open with DELETE access; `gone` is live for
        // the call and as large as the length passed.
        unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&raw const gone).cast(),
                size_of::<FILE_DISPOSITION_INFO>() as u32,
            );
        }
    }
    written
}

/// Give the open `file` the name `name` in the open `directory`, over any
/// file of that name there: NT looks the name up from the directory's
/// handle, not from a path. (Win32's `SetFileInformationByHandle` would
/// turn the name into a path from the current directory first.)
fn rename_in(directory: &File, file: &File, name: &OsStr) -> io::Result<()> {
    let name: Vec<u16> = name.encode_wide().collect();
    let bytes = name.len() * 2;
    let size = size_of::<FILE_RENAME_INFORMATION>() + bytes;
    let length = u32::try_from(size).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "cache name too long")
    })?;
    // In words: aligned as the struct's handle is.
    let mut buffer = vec![0_u64; size.div_ceil(8)];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: `buffer` holds `size` bytes aligned for the struct: its fixed
    // part, then room for the name from `FileName` on. No reference is
    // made, so the name may run past the declared one-unit array.
    unsafe {
        (&raw mut (*info).RootDirectory).write(directory.as_raw_handle());
        (&raw mut (*info).FileNameLength).write(bytes as u32);
        (&raw mut (*info).FileName)
            .cast::<u16>()
            .copy_from_nonoverlapping(name.as_ptr(), name.len());
    }
    // POSIX semantics replace a cache a reader still holds open; a file
    // system without them gets the plain rename.
    let mut result = 0;
    for (class, flags) in [
        (FileRenameInformationEx, RENAME_REPLACE | RENAME_POSIX),
        (FileRenameInformation, RENAME_REPLACE),
    ] {
        let mut status = IO_STATUS_BLOCK::default();
        // SAFETY: as above; `Flags` shares its first byte with
        // `ReplaceIfExists`. The handle is open with DELETE access, and
        // `status` is a live local.
        result = unsafe {
            (&raw mut (*info).Anonymous.Flags).write(flags);
            NtSetInformationFile(
                file.as_raw_handle(),
                &raw mut status,
                info.cast_const().cast(),
                length,
                class,
            )
        };
        if result >= 0 {
            return Ok(());
        }
    }
    Err(nt_error(result))
}

fn nt_error(status: i32) -> io::Error {
    // SAFETY: only translates a status code.
    let code = unsafe { RtlNtStatusToDosError(status) };
    io::Error::from_raw_os_error(code.cast_signed())
}

/// Bytes aligned to a page, for reads that bypass the file cache: those
/// want memory aligned to the disk's sector, which a page always is.
#[derive(Debug, Default)]
pub struct Aligned {
    pages: Vec<Page>,
}

#[derive(Clone, Copy, Debug)]
#[repr(C, align(4096))]
struct Page([u8; 4096]);

impl Aligned {
    /// `len` bytes, their contents whatever the last use left.
    pub fn bytes(&mut self, len: usize) -> &mut [u8] {
        let pages = len.div_ceil(size_of::<Page>());
        if self.pages.len() < pages {
            self.pages.resize(pages, Page([0; 4096]));
        }
        // SAFETY: `Page` is a `repr(C)` byte array, so the pages are one
        // allocation of initialized bytes with no padding, and `len` fits
        // in it; the borrow of `self` keeps it alive and unaliased.
        unsafe {
            std::slice::from_raw_parts_mut(
                self.pages.as_mut_ptr().cast::<u8>(),
                len,
            )
        }
    }
}

/// Make a directory junction for a test: unlike a symbolic link, one needs
/// no privilege to create. `false` when there is no `cmd` to make it with.
#[cfg(test)]
pub fn make_junction(link: &Path, target: &Path) -> bool {
    // Rebuilt from components, because `mklink` takes no `/` in a path, and
    // a test's `root.join("sub/loop")` has one.
    let native = |path: &Path| path.components().collect::<PathBuf>();
    std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(native(link))
        .arg(native(target))
        .output()
        .is_ok_and(|output| output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn elevated_cache_refuses_untrusted_owner_and_write_grants() {
        let trusted = cache_security().expect("admin descriptor");
        assert!(trusted_descriptor(trusted.0));
        for good in [
            "O:SYG:SYD:P(A;;FA;;;SY)",
            // Reading is anyone's; denials and entries only for what is
            // made inside later take nothing from the admins' hold.
            "O:BAG:BAD:P(A;;FA;;;BA)(A;;FR;;;BU)",
            "O:BAG:BAD:P(A;;FA;;;BA)(D;;FA;;;BU)",
            "O:BAG:BAD:P(A;;FA;;;BA)(A;OICIIO;GA;;;BU)",
        ] {
            let descriptor = descriptor_from_sddl(good).expect(good);
            assert!(trusted_descriptor(descriptor.0), "{good}");
        }
        for bad in [
            "O:BUG:BAD:P(A;;FA;;;BA)",
            "O:BAG:BAD:P(A;;FA;;;BA)(A;;GW;;;BU)",
            "O:BAG:BAD:P(A;;FA;;;BA)(A;;GA;;;AU)",
            "O:BAG:BAD:P(A;;FA;;;BA)(A;;SD;;;BU)",
            "O:BAG:BAD:P(A;;FA;;;BA)(A;;WD;;;WD)",
            "O:BAG:BAD:P(A;;FA;;;BA)(A;CI;FA;;;BU)",
            "O:BAG:BAD:(A;;FA;;;BA)",
            "O:BAG:BA",
        ] {
            let descriptor = descriptor_from_sddl(bad).expect(bad);
            assert!(!trusted_descriptor(descriptor.0), "{bad}");
        }
        // An ordinary file, owned by the user (or by Administrators when
        // elevated) under the ACL it inherits: never trusted, yet a
        // process that is not elevated reads it as its own.
        let temp = TempDir::new().expect("tempdir");
        let file = temp.path().join("user-owned.bin");
        fs::write(&file, b"forged tree").expect("write");
        let handle = OpenOptions::new()
            .access_mode(READ_CONTROL)
            .open(&file)
            .expect("open");
        assert!(!trusted_handle(&handle));
        assert!(read_cache(&file, true).is_none());
        assert!(read_cache(&file, false).is_some());
    }

    #[test]
    fn a_failed_cache_write_leaves_the_last_one_and_nothing_beside_it() {
        use std::io::{Read as _, Write as _};
        let temp = TempDir::new().expect("tempdir");
        // In a directory the write makes, as an elevated one must.
        let path = cache_path(&temp.path().join("cache"), "walk.bin");
        cache_write(&path, |out| out.write_all(b"whole")).expect("written");
        let failed = cache_write(&path, |out| {
            out.write_all(b"half")?;
            Err(io::Error::other("stopped"))
        });
        assert!(failed.is_err());
        let mut kept = String::new();
        cache_read(&path)
            .expect("kept")
            .read_to_string(&mut kept)
            .expect("read");
        assert_eq!(kept, "whole");
        let names: Vec<_> = fs::read_dir(path.parent().expect("dir"))
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["walk.bin"]);
    }

    #[test]
    fn quoted_arguments_survive_the_command_line() {
        let quote =
            |arg: &str| String::from_utf16(&quoted(OsStr::new(arg))).unwrap();
        // A drive root's trailing backslash would otherwise escape the quote.
        assert_eq!(quote(r"C:\"), r#""C:\\""#);
        assert_eq!(quote(r"C:\Program Files\x"), r#""C:\Program Files\x""#);
        assert_eq!(quote(r#"a\"b"#), r#""a\\\"b""#);
        assert_eq!(quote("--metric"), r#""--metric""#);
    }

    #[test]
    fn the_volume_list_has_the_system_drive() {
        let drive =
            std::env::var_os("SystemDrive").unwrap_or_else(|| "C:".into());
        let root = PathBuf::from(drive).join(Component::RootDir.as_os_str());
        let points = mount_points();
        assert!(
            points.iter().any(|point| same(point, &root)),
            "{} not in {points:?}",
            root.display()
        );
    }

    fn listed(dir: &Path) -> Vec<Entry> {
        let mut entries: Vec<Entry> = read_dir(dir, None)
            .expect("list")
            .collect::<io::Result<_>>()
            .expect("entries");
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        entries
    }

    #[test]
    fn a_listing_has_names_kinds_lengths_and_no_dot_entries() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("sub")).expect("mkdir");
        fs::write(temp.path().join("data.bin"), vec![7_u8; 100_000])
            .expect("write");

        let entries = listed(temp.path());
        let names: Vec<&OsStr> = entries.iter().map(Entry::file_name).collect();
        assert_eq!(names, ["data.bin", "sub"]);
        assert_eq!(entries[0].kind(), Kind::File);
        assert_eq!(entries[0].apparent(), 100_000);
        assert_eq!(entries[0].path(temp.path()), temp.path().join("data.bin"));
        assert_eq!(entries[1].kind(), Kind::Directory);
        assert!(entries[0].modified() > 0, "written just now");
    }

    #[test]
    fn a_name_that_is_not_text_is_shown_lossy_and_still_opens() {
        use std::os::windows::ffi::OsStringExt as _;
        let temp = TempDir::new().expect("tempdir");
        // An unpaired surrogate: NTFS takes it, text cannot hold it.
        let raw =
            OsString::from_wide(&[u16::from(b'a'), 0xD800, u16::from(b'b')]);
        fs::write(temp.path().join(&raw), b"x").expect("write");

        let entries = listed(temp.path());
        let [entry] = &entries[..] else {
            panic!("one entry: {}", entries.len());
        };
        assert_eq!(entry.name(), "a\u{FFFD}b");
        assert_eq!(entry.file_name(), raw);
        assert!(fs::metadata(entry.path(temp.path())).is_ok());
    }

    #[test]
    fn only_a_local_drive_gives_a_walk_one_volume() {
        let temp = TempDir::new().expect("tempdir");
        let canonical = temp.path().canonicalize().expect("canonical");
        assert!(walk_volume(&canonical).is_some(), "{}", canonical.display());
        // A share's DFS links lead to other volumes; nothing is asked.
        for share in [r"\\server\share\dir", r"\\?\UNC\server\share\dir"] {
            assert_eq!(walk_volume(Path::new(share)), None, "{share}");
        }
    }

    #[test]
    fn the_allocation_is_whole_clusters_of_the_data() {
        let temp = TempDir::new().expect("tempdir");
        // Noise, so a compressed folder cannot store it in less.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let noise: Vec<u8> = (0..100_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect();
        fs::write(temp.path().join("data.bin"), noise).expect("write");
        let entry = &listed(temp.path())[0];
        // Clusters are powers of two between 512 bytes and 2 MiB, so the
        // allocation covers the data and is a whole number of sectors.
        assert!(entry.allocated() >= 100_000, "{}", entry.allocated());
        assert!(entry.allocated() < 100_000 + 2 * 1024 * 1024);
        assert_eq!(entry.allocated() % 512, 0);
    }

    #[test]
    fn two_names_of_one_file_share_an_identity() {
        let temp = TempDir::new().expect("tempdir");
        let original = temp.path().join("original.bin");
        fs::write(&original, b"payload").expect("write");
        fs::hard_link(&original, temp.path().join("link.bin"))
            .expect("hard link");
        fs::write(temp.path().join("other.bin"), b"payload").expect("write");

        let entries = listed(temp.path());
        let id = |name: &str| {
            entries
                .iter()
                .find(|entry| entry.file_name() == name)
                .and_then(Entry::identity)
        };
        assert!(id("original.bin").is_some(), "NTFS gives ids");
        assert_eq!(id("link.bin"), id("original.bin"));
        assert_ne!(id("other.bin"), id("original.bin"));
        assert_eq!(identity(&original), id("original.bin"), "one id scheme");
    }

    #[test]
    fn a_junction_is_a_link_and_listing_it_lists_its_target() {
        let temp = TempDir::new().expect("tempdir");
        let target = temp.path().join("target");
        fs::create_dir(&target).expect("mkdir");
        fs::write(target.join("inside.bin"), b"x").expect("write");
        let junction = temp.path().join("junction");
        if !make_junction(&junction, &target) {
            return; // no cmd.exe to make one with
        }

        let entries = listed(temp.path());
        let kind = |name: &str| {
            entries
                .iter()
                .find(|entry| entry.file_name() == name)
                .map(Entry::kind)
        };
        assert_eq!(kind("junction"), Some(Kind::Link));
        assert_eq!(kind("target"), Some(Kind::Directory));
        let through: Vec<Box<str>> = listed(&junction)
            .into_iter()
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            &*through[0], "inside.bin",
            "a walk that enters one follows"
        );
        assert_eq!(through.len(), 1);
    }

    #[test]
    fn a_large_directory_spans_several_buffers() {
        let temp = TempDir::new().expect("tempdir");
        // Long names so the records overflow one 64 KiB buffer many times.
        for index in 0..2000 {
            let name = format!("{index:04}-{}", "n".repeat(120));
            fs::write(temp.path().join(name), b"").expect("write");
        }
        let entries = listed(temp.path());
        assert_eq!(entries.len(), 2000);
        assert!(
            entries[1999]
                .file_name()
                .to_string_lossy()
                .starts_with("1999")
        );
    }

    #[test]
    fn free_space_is_plausible_and_a_missing_path_is_not_found() {
        let temp = TempDir::new().expect("tempdir");
        let space = space_info(temp.path()).expect("space");
        assert!(space.total > 0 && space.free <= space.total, "{space:?}");
        assert!(space.available <= space.free, "{space:?}");
        let error = space_info(&temp.path().join("absent"))
            .expect_err("no such directory");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_path_is_on_its_drive_and_a_folder_is_no_mount_point() {
        let temp = TempDir::new().expect("tempdir");
        let drive = temp.path().ancestors().last().expect("a drive");
        let root = volume_root(temp.path()).expect("a volume");
        assert!(
            same(&root, drive),
            "{} on {}",
            root.display(),
            drive.display()
        );
        assert!(!is_mount_point(temp.path()));
        assert!(!file_table_readable(temp.path()), "a folder is walked");
        assert!(!is_mount_point(drive), "a drive is a root, not a folder");
    }

    /// Containment as the removal guards judge it.
    fn within(path: &Path, base: &Path) -> bool {
        guard_key(path).starts_with(guard_key(base))
    }

    #[test]
    fn paths_compare_without_case_or_verbatim_prefixes() {
        let windows = Path::new(r"C:\Windows");
        assert!(within(Path::new(r"c:\WINDOWS\System32"), windows));
        assert!(within(Path::new(r"\\?\C:\Windows\Temp"), windows));
        assert!(!within(Path::new(r"C:\Windows.old"), windows), "components");
        assert!(!within(Path::new(r"D:\Windows"), windows));
        assert!(same(
            Path::new(r"\\server\Share\Dir"),
            Path::new(r"\\?\UNC\SERVER\share\dir")
        ));
        assert!(within(
            Path::new(r"C:\Users\Олег\x"),
            Path::new(r"c:\users\ОЛЕГ")
        ));
    }

    #[test]
    fn times_convert_to_unix_seconds() {
        assert_eq!(unix_seconds(0), 0, "before 1970");
        assert_eq!(unix_seconds(EPOCH_TICKS), 0);
        assert_eq!(unix_seconds(EPOCH_TICKS + 90 * TICKS_PER_SECOND), 90);
    }

    #[test]
    fn only_whole_ids_name_a_file() {
        assert_eq!(file_id(0), None, "no id at all");
        assert_eq!(file_id(0x0005_0000_0000_1234), Some(0x0005_0000_0000_1234));
        assert_eq!(file_id((1 << 64) | 7), None, "a ReFS id needs both halves");
    }
}

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
use std::os::windows::io::AsRawHandle as _;
use std::path::{Component, Path, PathBuf, Prefix};

use windows_sys::Win32::Foundation::{
    ERROR_INVALID_FUNCTION, ERROR_INVALID_LEVEL, ERROR_INVALID_PARAMETER,
    ERROR_NO_MORE_FILES, ERROR_NOT_SUPPORTED, INVALID_HANDLE_VALUE, MAX_PATH,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS, FILE_ATTRIBUTE_RECALL_ON_OPEN,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_ID_EXTD_DIR_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FileIdExtdDirectoryInfo, FindFirstVolumeW, FindNextVolumeW,
    FindVolumeClose, GetDiskFreeSpaceExW, GetFileInformationByHandleEx,
    GetLogicalDriveStringsW, GetVolumeInformationW, GetVolumePathNameW,
    GetVolumePathNamesForVolumeNameW, SYNCHRONIZE,
};

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

    /// The name, moved out: the tree keeps it, so nothing copies it. The
    /// entry's other accessors keep working; `name`, `file_name` and
    /// `path` do not.
    pub fn take_name(&mut self) -> Box<str> {
        std::mem::take(&mut self.name)
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

/// Every drive letter and volume mount point. Logical drives include mapped
/// network letters that the local volume mount enumeration does not list.
pub fn mount_points() -> Vec<PathBuf> {
    let mut points = logical_drive_roots();
    // A volume GUID path, `\\?\Volume{…}\`, is 49 units with its NUL.
    let mut volume = [0_u16; 64];
    let length = u32::try_from(volume.len()).unwrap_or(u32::MAX);
    // SAFETY: `volume` is writable for the length passed.
    let find = unsafe { FindFirstVolumeW(volume.as_mut_ptr(), length) };
    if find.is_null() || find == INVALID_HANDLE_VALUE {
        return points;
    }
    loop {
        for point in volume_paths(&volume) {
            if !points.iter().any(|known| same(known, &point)) {
                points.push(point);
            }
        }
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

fn logical_drive_roots() -> Vec<PathBuf> {
    // SAFETY: a zero length asks for the buffer size without writing data.
    let needed = unsafe { GetLogicalDriveStringsW(0, std::ptr::null_mut()) };
    let Some(length) =
        usize::try_from(needed).ok().filter(|&length| length > 0)
    else {
        return Vec::new();
    };
    let mut names = vec![0_u16; length.saturating_add(1)];
    let Ok(capacity) = u32::try_from(names.len()) else {
        return Vec::new();
    };
    // SAFETY: `names` is writable for `capacity` UTF-16 units.
    let copied =
        unsafe { GetLogicalDriveStringsW(capacity, names.as_mut_ptr()) };
    if copied == 0
        || usize::try_from(copied).unwrap_or(usize::MAX) >= names.len()
    {
        return Vec::new();
    }
    names
        .split(|&unit| unit == 0)
        .take_while(|name| !name.is_empty())
        .map(|name| PathBuf::from(OsString::from_wide(name)))
        .collect()
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
    let Some(letter) = drive_letter(&canonical) else {
        return false;
    };
    let volume: Vec<u16> = format!("{letter}:\\\0").encode_utf16().collect();
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

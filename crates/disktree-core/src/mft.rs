//! Measuring an NTFS volume from its master file table, the way `WizTree`
//! does: one read of every file record instead of a listing per directory.
//! On a disk of four million files the walk opens six hundred thousand
//! directories; this reads five gigabytes in a few large requests.
//!
//! Only for a whole drive, `C:\`. The table costs the same to read whatever
//! part of it is wanted, so for a small directory the walk is faster.
//!
//! The table is read as it is on disk. NTFS writes changed records back
//! lazily, so changes other programs made in the last few seconds may not
//! show yet. disktree flushes the volume after its own removals, so a
//! rescan right after one sees what went. Flushing here instead would
//! need a write-capable volume open on every scan, which Controlled
//! Folder Access blocks and reports.
//!
//! Reading the volume needs an administrator. Anything this cannot do,
//! from a refused open to a record layout it does not expect, returns
//! `None` and the walk measures instead, so this is only ever a faster way
//! to the same tree.
//!
//! With [`ScanOptions::cache`] set, the finished tree one read made is
//! kept for the next scan, which loads it, reads again only the files the
//! volume's change journal names, and changes only what those touch: see
//! `snapshot.rs` and `flat.rs`.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::{FileExt as _, OpenOptionsExt as _};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rayon::prelude::*;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
    FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_NO_BUFFERING, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE,
};

use crate::scan::{ScanOptions, ScanProgress};
use crate::tree::{Seen, Tree};
use crate::windows::{Aligned, drive_letter};

mod flat;
mod snapshot;

use snapshot::{Checkpoint, Journal, State};

/// Most bytes read per call: large enough that the disk streams, small
/// enough that many reads are outstanding at once.
const CHUNK_BYTES: u64 = 16 << 20;

/// Free bytes a read spans rather than stop and start another. Through
/// `BitLocker`, 16 threads reading 1 MiB each moved 2 GB/s and 16 MiB
/// each 4.2 GB/s: a request costs about 0.29 ms on top of 0.23 ms per
/// MiB, as much as reading 1.3 MiB more, so a shorter free stretch is
/// cheaper read than skipped. On a file of synthetic records with free
/// stretches of about 600 records every 2000, reading through them took
/// 1028 ms against 1209 ms for stopping at 512 records; on a table
/// without free stretches the two plans are the same. An elevated scan of
/// a 4-million-record `C:\` took 3.44 s at 1 MiB against 3.60 s at 4 MiB
/// (median of 6 alternating runs).
const GAP_BYTES: u64 = 1 << 20;

/// The root directory's record number, fixed by the format.
const ROOT: u32 = 5;

/// Directory levels the table is followed down. A real tree is far
/// shallower; a corrupt one can loop, a directory beneath itself, and past
/// this the walk measures instead.
const MOST_LEVELS: usize = 1024;

/// Most bytes [`contents`] reads: the table's `$ATTRIBUTE_LIST` and
/// `$BITMAP` are kilobytes to a few megabytes, and a length beyond this is
/// a corrupt record, not something to allocate.
const MOST_CONTENTS: u64 = 64 << 20;

/// Records below this are the file system's own ($MFT, $Bitmap, ...): a
/// directory listing never shows them, so neither does this.
const FIRST_USER_RECORD: u32 = 16;

const IN_USE: u16 = 0x1;
const IS_DIRECTORY: u16 = 0x2;

const STANDARD_INFORMATION: u32 = 0x10;
const ATTRIBUTE_LIST: u32 = 0x20;
const FILE_NAME: u32 = 0x30;
const DATA: u32 = 0x80;
const BITMAP: u32 = 0xB0;
const REPARSE_POINT: u32 = 0xC0;
const END: u32 = 0xFFFF_FFFF;

/// The 8.3 alias of a name that has a long one: the same entry again.
const DOS_NAMESPACE: u8 = 2;

/// See `windows.rs`: links, junctions and mounted folders.
const NAME_SURROGATE: u32 = 0x2000_0000;
const EVICTED: u32 =
    FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;

/// What one base file record says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Info {
    in_use: bool,
    directory: bool,
    hidden: bool,
    evicted: bool,
    reparse: u8,
    /// Bumped each time the record is reused: a reference carrying another
    /// one names a file that is gone.
    sequence: u16,
    modified: i64,
    apparent: u64,
    allocated: u64,
    names: u8,
}

impl Info {
    const REPARSE: u8 = 1;
    const TAGGED: u8 = 2;
    const SURROGATE: u8 = 4;

    const fn set_attributes(&mut self, attributes: u32) {
        self.hidden = attributes & FILE_ATTRIBUTE_HIDDEN != 0;
        self.evicted = attributes & EVICTED != 0;
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            self.reparse |= Self::REPARSE;
        } else {
            self.reparse &= !Self::REPARSE;
        }
    }

    const fn has_tag(&self) -> bool {
        self.reparse & Self::TAGGED != 0
    }

    const fn is_link(&self) -> bool {
        self.reparse & (Self::REPARSE | Self::SURROGATE)
            == (Self::REPARSE | Self::SURROGATE)
    }

    /// Presence must survive even for non-surrogate tags: otherwise a
    /// stale name hint or extension could turn a directory into a link.
    const fn set_tag(&mut self, tag: u32) {
        self.reparse &= !(Self::TAGGED | Self::SURROGATE);
        if tag != 0 {
            self.reparse |= Self::TAGGED;
        }
        if tag & NAME_SURROGATE != 0 {
            self.reparse |= Self::SURROGATE;
        }
    }
}

/// Empty stretches of the MFT need no record storage. A page lookup
/// keeps access constant-time without a pointer allocation per record.
#[derive(Default)]
struct RecordTable {
    pages: Vec<usize>,
    values: Vec<Info>,
    len: usize,
}

impl RecordTable {
    const PAGE: usize = 256;
    const EMPTY: usize = usize::MAX;

    fn offset(pages: &[usize], number: usize) -> Option<usize> {
        let &page = pages.get(number / Self::PAGE)?;
        (page != Self::EMPTY).then(|| page + number % Self::PAGE)
    }

    const fn len(&self) -> usize {
        self.len
    }

    fn get(&self, number: usize) -> Option<&Info> {
        if number >= self.len {
            return None;
        }
        self.values.get(Self::offset(&self.pages, number)?)
    }

    fn get_mut(&mut self, number: usize) -> Option<&mut Info> {
        if number >= self.len {
            return None;
        }
        let at = Self::offset(&self.pages, number)?;
        self.values.get_mut(at)
    }

    /// Allocate all new pages together before a journal patch writes any
    /// records, so growth copies the existing values at most once.
    fn reserve(
        &mut self,
        ranges: impl IntoIterator<Item = std::ops::Range<usize>>,
    ) {
        let mut count = self.values.len();
        for range in ranges {
            self.len = self.len.max(range.end);
            self.pages
                .resize(self.len.div_ceil(Self::PAGE), Self::EMPTY);
            for page in &mut self.pages
                [range.start / Self::PAGE..range.end.div_ceil(Self::PAGE)]
            {
                if *page == Self::EMPTY {
                    *page = count;
                    count += Self::PAGE;
                }
            }
        }
        let added = count - self.values.len();
        self.values.reserve_exact(added);
        self.values
            .par_extend(rayon::iter::repeat_n(Info::default(), added));
    }
}

#[cfg(test)]
impl From<Vec<Info>> for RecordTable {
    fn from(values: Vec<Info>) -> Self {
        let mut table = Self::default();
        table.reserve(values.chunks(Self::PAGE).enumerate().filter_map(
            |(page, values)| {
                values
                    .iter()
                    .any(|info| info.in_use)
                    .then_some(page * Self::PAGE..(page + 1) * Self::PAGE)
            },
        ));
        table.len = values.len();
        for (number, info) in values.into_iter().enumerate() {
            if let Some(slot) = table.get_mut(number) {
                *slot = info;
            }
        }
        table
    }
}

/// One name of a file: an entry in its parent directory. The name itself
/// is `len` bytes at `at` in its chunk's text: millions of names in a few
/// strings, not millions of allocations made only to be copied again.
#[derive(Clone, Copy)]
struct Entry {
    parent: u32,
    parent_sequence: u16,
    child: u32,
    chunk: u32,
    at: u32,
    len: u16,
}

impl Entry {
    /// A placeholder a list is filled with before its entries are copied in.
    const EMPTY: Self = Self {
        parent: 0,
        parent_sequence: 0,
        child: 0,
        chunk: 0,
        at: 0,
        len: 0,
    };
}

/// What one chunk of records contributes besides its stretch of the
/// table.
#[derive(Default)]
struct Parsed {
    names: Vec<Entry>,
    text: String,
    /// Files among the records, and what their base records say they
    /// hold, for the progress meter: `settle` corrects both at the end.
    files: u64,
    apparent: u64,
    allocated: u64,
    /// What extension records hold for their base record: `(base, base
    /// sequence, apparent size, bytes allocated, reparse tag)`.
    extra: Vec<(u32, u16, Option<u64>, u64, u32)>,
    /// Names found in extension records, with their base's sequence: kept
    /// apart since they are rare and must be checked against the base.
    extra_names: Vec<(Entry, u16)>,
}

/// Where a stream lies on the volume: `(byte offset, byte length)` runs.
type Runs = Vec<(u64, u64)>;

struct Geometry {
    cluster: u64,
    record: usize,
    mft_offset: u64,
    /// The volume's size in bytes: no table can be larger.
    volume: u64,
    /// The serial number the volume was formatted with.
    serial: u64,
}

/// The tree under `root`, read from the volume's file table; `None` when
/// the table cannot be read and the walk has to measure instead. The tree
/// comes back finished: totalled, ordered and classified. It is the tree
/// kept for the next scan too, written by a thread of its own that shares
/// it rather than a copy.
pub fn scan(
    root: &Path,
    canonical: &Path,
    options: &ScanOptions,
    progress: &ScanProgress,
) -> Option<io::Result<Arc<Tree>>> {
    // A followed link can lead off this volume, or back into it a second
    // time, and the table describes neither: the walk follows them.
    if options.follow_links {
        return None;
    }
    let letter = drive_letter(canonical)?;
    let path = format!(r"\\.\{letter}:");
    // Its file must not change under the read of it next.
    crate::scan::wait_for_cache();
    let volume = open_volume(&path).ok()?;
    let geometry = geometry(&volume)?;
    // A depth limit leaves out directories a later change can bring into
    // view, with entries a kept tree would not hold: nothing is kept.
    let file = options
        .cache
        .as_deref()
        .filter(|_| options.max_depth.is_none())
        .map(|dir| snapshot::file(dir, letter));
    let journal = file.as_ref().and_then(|_| snapshot::query(&volume));
    let resumed = file.as_deref().zip(journal).and_then(|(file, journal)| {
        snapshot::resume(&path, &volume, &geometry, journal, file, options)
    });
    let (tree, checkpoint) = match resumed {
        Some((mut tree, checkpoint)) => {
            tree.rename(crate::scan::file_name(root));
            (Ok(Arc::new(tree)), checkpoint)
        }
        None => {
            match whole(&path, &volume, &geometry, journal, options, progress)?
            {
                Ok((mut tree, checkpoint)) => {
                    tree.rename(crate::scan::file_name(root));
                    crate::classify::classify(&mut tree);
                    (Ok(Arc::new(tree)), checkpoint)
                }
                Err(stop) => (Err(stop), None),
            }
        }
    };
    if let (Ok(tree), Some(file), Some(checkpoint)) = (&tree, file, checkpoint)
        && !progress.is_cancelled()
    {
        let tree = Arc::clone(tree);
        snapshot::save_later(file, &geometry, options, checkpoint, move || {
            // What patches left behind is not worth keeping; nor is a tree
            // too large for one segment kept at all.
            if tree.has_garbage() {
                tree.compact().map(Arc::new)
            } else {
                Some(tree)
            }
        });
    }
    match tree {
        Ok(_) | Err(Stop) if progress.is_cancelled() => Some(Err(cancelled())),
        Ok(tree) => Some(Ok(tree)),
        Err(Stop) => None,
    }
}

/// What [`whole`] gives: the tree, not yet classified, and the checkpoint.
type Whole = (Tree, Option<Checkpoint>);

/// The directories changed just before a whole read, with the checkpoint.
type Recent = (Checkpoint, Vec<u32>);

/// The tree from a whole read of the table, not yet classified, and with
/// `journal`, where the next scan can pick up. `None` when the table
/// cannot be read, and [`Stop`] when the read was cancelled or the table
/// makes no tree.
fn whole(
    path: &str,
    volume: &File,
    geometry: &Geometry,
    journal: Option<Journal>,
    options: &ScanOptions,
    progress: &ScanProgress,
) -> Option<Result<Whole, Stop>> {
    let (mut state, checkpoint) =
        read_whole(path, volume, geometry, journal, options, progress)?;
    if progress.is_cancelled() {
        return Some(Err(Stop));
    }
    if !is_root_directory(&state.infos) {
        return None;
    }
    state.names.par_sort_unstable_by_key(|name| name.parent);
    let checkpoint = checkpoint.map(|(checkpoint, recent)| {
        // A directory changed just before the read can be on the disk as
        // it was, or not at all, and what is in it would then find no
        // place in the tree the next scan starts from. Read again from
        // NTFS, which has them as they are; their files are read again on
        // the next scan, with the rest of what changed.
        let _ = snapshot::reread(path, geometry, &mut state, &recent);
        checkpoint
    });
    let starts = starts(&state.names, state.infos.len());
    let table = Table::of(state, starts, options, progress);
    let tree = table.build();
    // A few large lists, and nothing waits on their freeing.
    let parts = table.into_parts();
    let _ = std::thread::Builder::new().spawn(move || drop(parts));
    Some(tree.map(|tree| (tree, checkpoint)))
}

/// Every used record of the table, read from the disk; with `journal`,
/// also where the next scan can pick up from, and the directories changed
/// just before the read.
fn read_whole(
    path: &str,
    volume: &File,
    geometry: &Geometry,
    journal: Option<Journal>,
    options: &ScanOptions,
    progress: &ScanProgress,
) -> Option<(State, Option<Recent>)> {
    let (runs, bitmap) = table_layout(volume, geometry)?;
    let reads = plan_reads(
        &runs,
        &bitmap,
        geometry,
        GAP_BYTES / geometry.record as u64,
    );
    let mut infos = table(&reads, geometry.record);
    // The journal, tens of megabytes read through NTFS, on a thread of its
    // own while the table is on the disk.
    let (parsed, checkpoint) = std::thread::scope(|scope| {
        let checkpoint = journal.map(|journal| {
            scope.spawn(move || snapshot::after_whole_read(volume, journal))
        });
        let parsed = read_records(
            path,
            geometry.record,
            &reads,
            &mut infos,
            options.apparent_size,
            progress,
        );
        let checkpoint = checkpoint.and_then(|thread| thread.join().ok()?);
        (parsed, checkpoint)
    });
    let (names, texts) = merge(parsed?, &mut infos);
    Some((
        State {
            infos,
            names,
            texts,
        },
        checkpoint,
    ))
}

/// Whether the root's record reads as an in-use directory; if not, the
/// table is not what this expects and the walk measures instead.
fn is_root_directory(infos: &RecordTable) -> bool {
    infos
        .get(ROOT as usize)
        .is_some_and(|root| root.in_use && root.directory)
}

pub fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "the scan was cancelled")
}

/// Past the file cache: the table is read once and never again, and
/// through the cache it is copied twice and evicts what the rest of the
/// system is using.
fn open_volume(path: &str) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_NO_BUFFERING)
        .open(path)
}

/// Where each parent's entries start in `names`, sorted by parent:
/// `starts[p]..starts[p + 1]`.
fn starts(names: &[Entry], records: usize) -> Vec<u32> {
    let mut starts = vec![0_u32; records + 1];
    for name in names {
        if let Some(count) = starts.get_mut(name.parent as usize + 1) {
            *count += 1;
        }
    }
    for index in 1..starts.len() {
        starts[index] += starts[index - 1];
    }
    starts
}

fn geometry(volume: &File) -> Option<Geometry> {
    geometry_of(&read_at(volume, 0, 512)?)
}

/// The sizes the boot sector gives, each checked: a corrupt one would
/// otherwise overflow a shift or allocate without bound.
fn geometry_of(boot: &[u8]) -> Option<Geometry> {
    if boot.get(3..11)? != b"NTFS    " {
        return None;
    }
    let sector = u64::from(u16_at(boot, 0x0B)?);
    let per_cluster = *boot.get(0x0D)?;
    // Above 0x80 the count is a negative power of two.
    let sectors = if per_cluster > 0x80 {
        1_u64.checked_shl(256 - u32::from(per_cluster))?
    } else {
        u64::from(per_cluster)
    };
    let cluster = sector.checked_mul(sectors)?;
    let per_record = boot.get(0x40)?.cast_signed();
    let record = if per_record < 0 {
        1_u64.checked_shl(u32::from(per_record.unsigned_abs()))?
    } else {
        cluster.checked_mul(u64::from(per_record.unsigned_abs()))?
    };
    let mft_offset = u64_at(boot, 0x30)?.checked_mul(cluster)?;
    let volume = u64_at(boot, 0x28)?.checked_mul(sector)?;
    let valid = sector.is_power_of_two()
        && (512..=4096).contains(&sector)
        && cluster.is_power_of_two()
        && cluster <= 2 << 20
        && record.is_power_of_two()
        && (1024..=4096).contains(&record);
    valid.then_some(Geometry {
        cluster,
        record: usize::try_from(record).ok()?,
        mft_offset,
        volume,
        serial: u64_at(boot, 0x48)?,
    })
}

/// Where the file table lies on disk, as `(byte offset, byte length)` runs,
/// and its `$BITMAP`: which records are in use. A table grows and never
/// shrinks, so on a disk that has seen churn most of it can be free; only
/// the used part is read.
///
/// Both come from the table's own first record, or from the records its
/// `$ATTRIBUTE_LIST` names once it has outgrown that one.
fn table_layout(volume: &File, geometry: &Geometry) -> Option<(Runs, Vec<u8>)> {
    let mut records =
        vec![read_record_at(volume, geometry, geometry.mft_offset)?];
    if attributes(&records[0]).any(|attribute| attribute.kind == ATTRIBUTE_LIST)
    {
        let list = contents(volume, geometry, &records, ATTRIBUTE_LIST)?;
        let (_, first) = stream(&records, DATA, geometry.cluster)?;
        let mut numbers = Vec::new();
        let mut at = 0;
        while let (Some(length), Some(reference)) =
            (u16_at(&list, at + 4), u64_at(&list, at + 0x10))
        {
            let number = reference & 0xFFFF_FFFF_FFFF;
            if number != 0 && !numbers.contains(&number) {
                numbers.push(number);
            }
            if length == 0 {
                break;
            }
            at += usize::from(length);
        }
        // The records holding the rest of the table's attributes are
        // early in it, inside what the first record already maps.
        for number in numbers {
            let offset = record_offset(&first, geometry, number)?;
            records.push(read_record_at(volume, geometry, offset)?);
        }
    }

    let (data, runs) = stream(&records, DATA, geometry.cluster)?;
    let listed = runs
        .iter()
        .try_fold(0_u64, |sum, &(_, length)| sum.checked_add(length))?;
    let record_bytes = geometry.record as u64;
    // Record numbers are 32 bits here, and `plan_reads` counts records
    // across runs whose ends `decode_runs` has checked fit. A table larger
    // than its volume is corrupt, and `merge` would allocate a slot per
    // record it claims.
    if listed < u64_at(data.bytes, 0x28)?
        || listed > geometry.volume
        || listed / record_bytes > u64::from(u32::MAX)
        || runs.iter().any(|(_, length)| length % record_bytes != 0)
    {
        return None;
    }
    let bitmap = contents(volume, geometry, &records, BITMAP)?;
    Some((runs, bitmap))
}

/// The unnamed attribute `kind` across `records`: the piece that starts
/// it, which carries its sizes, and the runs of every piece in order.
fn stream(
    records: &[Vec<u8>],
    kind: u32,
    cluster: u64,
) -> Option<(Attribute<'_>, Runs)> {
    let mut pieces: Vec<Attribute<'_>> = records
        .iter()
        .flat_map(|record| attributes(record))
        .filter(|attribute| {
            attribute.kind == kind && attribute.name_length == 0
        })
        .collect();
    pieces.sort_by_key(|piece| {
        if piece.non_resident {
            u64_at(piece.bytes, 0x10).unwrap_or(u64::MAX)
        } else {
            0
        }
    });
    let mut runs = Vec::new();
    for piece in pieces.iter().filter(|piece| piece.non_resident) {
        runs.extend(decode_runs(piece.bytes, cluster)?);
    }
    let first = pieces.into_iter().next()?;
    Some((first, runs))
}

/// All of the unnamed attribute `kind`'s value, wherever it is kept.
/// `None` when what the runs hold falls short of its length: for the
/// table's bitmap every record past the end would be skipped as free.
fn contents(
    volume: &File,
    geometry: &Geometry,
    records: &[Vec<u8>],
    kind: u32,
) -> Option<Vec<u8>> {
    let (first, runs) = stream(records, kind, geometry.cluster)?;
    if !first.non_resident {
        return first.value().map(<[u8]>::to_vec);
    }
    let length = u64_at(first.bytes, 0x30)?;
    let held = runs
        .iter()
        .try_fold(0_u64, |sum, &(_, size)| sum.checked_add(size))?;
    if length > MOST_CONTENTS || held > MOST_CONTENTS {
        return None;
    }
    let length = usize::try_from(length).ok()?;
    // The table's bitmap grows with it, a few clusters at a time: hundreds
    // of runs, which read one after another cost tens of milliseconds.
    let pieces: Vec<Vec<u8>> = runs
        .into_par_iter()
        .map(|(offset, size)| {
            read_at(volume, offset, usize::try_from(size).ok()?)
        })
        .collect::<Option<_>>()?;
    let mut bytes = pieces.concat();
    (bytes.len() >= length).then_some(())?;
    bytes.truncate(length);
    Some(bytes)
}

/// Where record `number` lies, given the runs that hold it.
fn record_offset(
    runs: &[(u64, u64)],
    geometry: &Geometry,
    number: u64,
) -> Option<u64> {
    let mut position = number.checked_mul(geometry.record as u64)?;
    for &(offset, length) in runs {
        if position < length {
            return offset.checked_add(position);
        }
        position -= length;
    }
    None
}

/// One record at `offset`, fixed up.
fn read_record_at(
    volume: &File,
    geometry: &Geometry,
    offset: u64,
) -> Option<Vec<u8>> {
    let mut record = read_at(volume, offset, geometry.record)?;
    fixup(&mut record)?;
    Some(record)
}

/// The reads that cover every used record: `(byte offset, byte length,
/// first record number)`. A free stretch of fewer than `gap_records` is
/// read through, since one longer read costs less than two (see
/// [`GAP_BYTES`]). Reads start and end on cluster boundaries, which a
/// volume handle requires of them.
fn plan_reads(
    runs: &[(u64, u64)],
    bitmap: &[u8],
    geometry: &Geometry,
    gap_records: u64,
) -> Vec<(u64, u64, u64)> {
    let used = |number: u64| {
        bitmap
            .get((number / 8) as usize)
            .is_some_and(|byte| byte & (1 << (number % 8)) != 0)
    };
    let record = geometry.record as u64;
    let align = (geometry.cluster / record).max(1);
    let most = CHUNK_BYTES / record;
    let mut reads = Vec::new();
    let mut first = 0_u64;
    for &(offset, length) in runs {
        let count = length / record;
        let mut at = 0;
        while at < count {
            if !used(first + at) {
                at += 1;
                continue;
            }
            let start = at - at % align;
            let mut end = at + 1;
            let mut probe = end;
            while probe < count
                && probe - start < most
                && probe - end < gap_records
            {
                if used(first + probe) {
                    end = probe + 1;
                }
                probe += 1;
            }
            let end = end.next_multiple_of(align).min(count);
            reads.push((
                offset + start * record,
                (end - start) * record,
                first + start,
            ));
            at = end;
        }
        first += count;
    }
    reads
}

/// The runs of a non-resident attribute. Each run's end fits in a `u64`,
/// which `plan_reads` relies on.
fn decode_runs(attribute: &[u8], cluster: u64) -> Option<Runs> {
    let mut at = usize::from(u16_at(attribute, 0x20)?);
    let mut lcn = 0_i64;
    let mut runs = Vec::new();
    loop {
        let header = *attribute.get(at)?;
        if header == 0 {
            return Some(runs);
        }
        let length_size = usize::from(header & 0xF);
        let offset_size = usize::from(header >> 4);
        let length = unsigned(attribute.get(at + 1..at + 1 + length_size)?);
        let delta = attribute
            .get(at + 1 + length_size..at + 1 + length_size + offset_size)?;
        at += 1 + length_size + offset_size;
        // No offset marks a sparse run, which a file table never has.
        if offset_size == 0 || offset_size > 8 || length_size > 8 {
            return None;
        }
        lcn = lcn.checked_add(signed(delta))?;
        let offset = u64::try_from(lcn).ok()?.checked_mul(cluster)?;
        let length = length.checked_mul(cluster)?;
        offset.checked_add(length)?;
        runs.push((offset, length));
    }
}

/// Only pages touched by a read need slots. Reads are ordered by record,
/// so each read remains one contiguous stretch of the compact values.
fn table(reads: &[(u64, u64, u64)], record: usize) -> RecordTable {
    let mut infos = RecordTable::default();
    infos.reserve(reads.iter().map(|&(_, size, first)| {
        let first = usize::try_from(first).unwrap_or(0);
        first..first + usize::try_from(size).unwrap_or(0) / record
    }));
    infos
}

/// Read and parse every used record, each into its slot of `infos`.
/// Reads run in parallel: one at a time leaves a solid-state disk idle
/// between requests. One worker per thread, each with its own volume
/// handle and buffer: requests on one handle opened without overlapped
/// I/O run one at a time, and rayon's `map_init` would open a handle and
/// zero a buffer per split, hundreds of times. Workers take reads from one
/// queue, largest first: dealt out in advance, one worker's share ran on
/// after the others were done, and a 16 MiB read taken last kept every
/// other thread waiting on it. Results come back in read order, which
/// `merge` relies on.
///
/// Records land in `infos` as they are parsed, while other reads are
/// still on the disk: gathered into it after the last read, they cost a
/// tenth of a second on the way to the tree.
fn read_records(
    path: &str,
    record: usize,
    reads: &[(u64, u64, u64)],
    infos: &mut RecordTable,
    apparent_size: bool,
    progress: &ScanProgress,
) -> Option<Vec<Parsed>> {
    let threads = rayon::current_num_threads().max(1);
    // `plan_reads` gives them in record order, none overlapping.
    let mut stretches = Vec::with_capacity(reads.len());
    let mut rest = infos.values.as_mut_slice();
    let mut done = 0;
    for &(_, size, first) in reads {
        let first =
            RecordTable::offset(&infos.pages, usize::try_from(first).ok()?)?;
        let skip = first.checked_sub(done)?;
        let count = usize::try_from(size).ok()? / record;
        let (stretch, tail) =
            rest.get_mut(skip..)?.split_at_mut_checked(count)?;
        stretches.push(Mutex::new(Some(stretch)));
        rest = tail;
        done += skip + count;
    }
    let mut order: Vec<usize> = (0..reads.len()).collect();
    order.sort_by_key(|&index| std::cmp::Reverse(reads[index].1));
    let next = AtomicUsize::new(0);
    let lists = (0..threads)
        .into_par_iter()
        .map(|_| {
            let volume = open_volume(path).ok()?;
            let mut buffer = Aligned::default();
            let mut parsed = Vec::new();
            while !progress.is_cancelled()
                && let Some(&index) =
                    order.get(next.fetch_add(1, Ordering::Relaxed))
            {
                let (offset, size, first) = reads[index];
                let stretch = crate::scan::lock(&stretches[index]).take()?;
                let buffer = buffer.bytes(usize::try_from(size).ok()?);
                volume.seek_read_exact(buffer, offset).ok()?;
                let chunk = parse_chunk(
                    buffer,
                    record,
                    first,
                    u32::try_from(index).ok()?,
                    stretch,
                );
                let bytes = if apparent_size {
                    chunk.apparent
                } else {
                    chunk.allocated
                };
                progress.add(chunk.files, 0, bytes);
                parsed.push((index, chunk));
            }
            Some(parsed)
        })
        .collect::<Option<Vec<_>>>()?;
    let mut slots: Vec<Option<Parsed>> =
        std::iter::repeat_with(|| None).take(reads.len()).collect();
    for (index, chunk) in lists.into_iter().flatten() {
        slots[index] = Some(chunk);
    }
    // Short only when cancelled, which the caller checks next.
    Some(slots.into_iter().map_while(|chunk| chunk).collect())
}

/// `len` bytes at `offset`. Read as whole, aligned 4 KiB blocks: past the
/// cache a volume only reads whole sectors into sector-aligned memory, and
/// no disk has sectors larger.
fn read_at(volume: &File, offset: u64, len: usize) -> Option<Vec<u8>> {
    const BLOCK: u64 = 4096;
    let start = offset - offset % BLOCK;
    let end = offset
        .checked_add(len as u64)?
        .checked_next_multiple_of(BLOCK)?;
    let mut buffer = Aligned::default();
    let block = buffer.bytes(usize::try_from(end - start).ok()?);
    volume.seek_read_exact(block, start).ok()?;
    let at = usize::try_from(offset - start).ok()?;
    block.get(at..at + len).map(<[u8]>::to_vec)
}

trait ReadExact {
    fn seek_read_exact(&self, buffer: &mut [u8], offset: u64)
    -> io::Result<()>;
}

impl ReadExact for File {
    fn seek_read_exact(
        &self,
        mut buffer: &mut [u8],
        mut offset: u64,
    ) -> io::Result<()> {
        while !buffer.is_empty() {
            match self.seek_read(buffer, offset)? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                read => {
                    buffer = &mut buffer[read..];
                    offset += read as u64;
                }
            }
        }
        Ok(())
    }
}

fn parse_chunk(
    buffer: &mut [u8],
    record: usize,
    first: u64,
    chunk: u32,
    infos: &mut [Info],
) -> Parsed {
    // About a name per record, and a name about 20 bytes: grown by
    // doubling instead, the lists are copied a dozen times per read.
    let records = buffer.len() / record;
    let mut out = Parsed {
        names: Vec::with_capacity(records),
        text: String::with_capacity(records * 24),
        ..Parsed::default()
    };
    for ((index, bytes), slot) in
        buffer.chunks_exact_mut(record).enumerate().zip(infos)
    {
        let Ok(number) = u32::try_from(first + index as u64) else {
            break;
        };
        parse_record(bytes, number, chunk, &mut out, slot);
    }
    out
}

/// A base record's facts go to `slot`; an extension record's to `out`.
fn parse_record(
    bytes: &mut [u8],
    number: u32,
    chunk: u32,
    out: &mut Parsed,
    slot: &mut Info,
) {
    if fixup(bytes).is_some() {
        parse_fixed(bytes, number, chunk, out, slot);
    }
}

/// [`parse_record`] for a record whose update sequence is already undone.
fn parse_fixed(
    bytes: &[u8],
    number: u32,
    chunk: u32,
    out: &mut Parsed,
    slot: &mut Info,
) {
    if bytes.get(..4) != Some(b"FILE") {
        return;
    }
    let Some(flags) = u16_at(bytes, 0x16) else {
        return;
    };
    if flags & IN_USE == 0 {
        return;
    }
    // A record holding attributes that did not fit in its base record.
    let base = u64_at(bytes, 0x20).unwrap_or(0);
    let (owner, base_sequence) = if base & REFERENCE == 0 {
        (number, 0)
    } else {
        let Some(reference) = reference(base) else {
            return;
        };
        reference
    };
    let mut info = Info {
        in_use: true,
        directory: flags & IS_DIRECTORY != 0,
        sequence: u16_at(bytes, 0x10).unwrap_or(0),
        ..Info::default()
    };
    let mut apparent = None;
    let mut allocated = 0_u64;
    let mut tag = 0;
    // A reparse point's tag is also kept beside each name, which serves
    // when the `$REPARSE_POINT` value is not in the record itself.
    let mut name_tag = 0;
    for attribute in attributes(bytes) {
        match attribute.kind {
            STANDARD_INFORMATION => {
                if let Some(value) = attribute.value() {
                    info.modified = u64_at(value, 0x08).map_or(0, |ticks| {
                        crate::windows::unix_seconds(ticks.cast_signed())
                    });
                    info.set_attributes(u32_at(value, 0x20).unwrap_or(0));
                }
            }
            FILE_NAME => {
                let Some(value) = attribute.value() else {
                    continue;
                };
                if u32_at(value, 0x38).unwrap_or(0)
                    & FILE_ATTRIBUTE_REPARSE_POINT
                    != 0
                {
                    name_tag = u32_at(value, 0x3C).unwrap_or(0);
                }
                let at = out.text.len();
                if let Some((parent, parent_sequence)) =
                    file_name(value, &mut out.text)
                {
                    let entry = Entry {
                        parent,
                        parent_sequence,
                        child: owner,
                        chunk,
                        at: u32::try_from(at).unwrap_or(u32::MAX),
                        len: u16::try_from(out.text.len() - at)
                            .unwrap_or(u16::MAX),
                    };
                    if base & REFERENCE == 0 {
                        out.names.push(entry);
                        info.names = info.names.saturating_add(1);
                    } else {
                        out.extra_names.push((entry, base_sequence));
                    }
                }
            }
            DATA => {
                // Every stream's clusters, named ones too: they go when the
                // file does. A file Windows compressed (`compact /exe`)
                // keeps its data in one and leaves the unnamed one empty.
                let (length, spent) = attribute.sizes();
                allocated = allocated.saturating_add(spent);
                // Only the first piece of the unnamed stream has its length.
                if attribute.name_length == 0 && attribute.starts_file() {
                    apparent = Some(length);
                }
            }
            REPARSE_POINT => {
                if let Some(value) = attribute.value() {
                    tag = u32_at(value, 0).unwrap_or(0);
                }
            }
            _ => {}
        }
    }
    if tag == 0 {
        tag = name_tag;
    }
    info.set_tag(tag);
    if base & REFERENCE == 0 {
        if !info.directory {
            out.files += 1;
            out.apparent = out.apparent.saturating_add(apparent.unwrap_or(0));
            out.allocated = out.allocated.saturating_add(allocated);
        }
        info.apparent = apparent.unwrap_or(0);
        info.allocated = allocated;
        *slot = info;
    } else if apparent.is_some() || allocated != 0 || tag != 0 {
        out.extra
            .push((owner, base_sequence, apparent, allocated, tag));
    }
}

/// The record number part of a file reference.
const REFERENCE: u64 = 0xFFFF_FFFF_FFFF;

/// A file reference: `(record number, sequence number)`.
fn reference(value: u64) -> Option<(u32, u16)> {
    let number = u32::try_from(value & REFERENCE).ok()?;
    let sequence = u16::try_from(value >> 48).ok()?;
    Some((number, sequence))
}

/// The parent reference of a `$FILE_NAME`, its name appended to `text`;
/// or `None`, and nothing appended, for an 8.3 alias or a name no
/// directory can hold, which as a path would name another file.
fn file_name(value: &[u8], text: &mut String) -> Option<(u32, u16)> {
    let parent = reference(u64_at(value, 0)?)?;
    let length = usize::from(*value.get(0x40)?);
    if *value.get(0x41)? == DOS_NAMESPACE {
        return None;
    }
    let units = value
        .get(0x42..0x42 + length * 2)?
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
    let start = text.len();
    text.extend(
        char::decode_utf16(units)
            .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER)),
    );
    let name = &text[start..];
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['\\', '/', '\0'])
    {
        text.truncate(start);
        return None;
    }
    Some(parent)
}

/// Undo the update sequence: the last two bytes of every sector were
/// swapped for a check value when the record was written.
fn fixup(bytes: &mut [u8]) -> Option<()> {
    // The update sequence always strides 512 bytes, whatever the sector.
    const STRIDE: usize = 512;
    let at = usize::from(u16_at(bytes, 4)?);
    let count = usize::from(u16_at(bytes, 6)?);
    if bytes.is_empty()
        || !bytes.len().is_multiple_of(STRIDE)
        || count != bytes.len() / STRIDE + 1
        || at + 2 * count > bytes.len()
    {
        return None;
    }
    let check = [bytes[at], bytes[at + 1]];
    for sector in 1..count {
        let end = sector * STRIDE;
        let replacement = [bytes[at + 2 * sector], bytes[at + 2 * sector + 1]];
        let tail = &mut bytes[end - 2..end];
        if tail != check {
            return None;
        }
        tail.copy_from_slice(&replacement);
    }
    Some(())
}

struct Attribute<'a> {
    kind: u32,
    non_resident: bool,
    name_length: u8,
    flags: u16,
    bytes: &'a [u8],
}

impl Attribute<'_> {
    fn value(&self) -> Option<&[u8]> {
        if self.non_resident {
            return None;
        }
        let length = u32_at(self.bytes, 0x10)? as usize;
        let at = usize::from(u16_at(self.bytes, 0x14)?);
        self.bytes.get(at..at + length)
    }

    fn starts_file(&self) -> bool {
        !self.non_resident || u64_at(self.bytes, 0x10) == Some(0)
    }

    /// `(length, bytes allocated)`. Data kept in the record costs nothing
    /// beyond the record; a compressed or sparse stream's real cost is its
    /// total allocation, which only the first piece carries.
    fn sizes(&self) -> (u64, u64) {
        if !self.non_resident {
            return (self.value().map_or(0, |value| value.len() as u64), 0);
        }
        if !self.starts_file() {
            return (0, 0);
        }
        let length = u64_at(self.bytes, 0x30).unwrap_or(0);
        let packed = self.flags & 0x8001 != 0;
        let allocated =
            u64_at(self.bytes, if packed { 0x40 } else { 0x28 }).unwrap_or(0);
        (length, allocated)
    }
}

fn attributes(record: &[u8]) -> impl Iterator<Item = Attribute<'_>> {
    let mut at = u16_at(record, 0x14).map_or(record.len(), usize::from);
    std::iter::from_fn(move || {
        let kind = u32_at(record, at)?;
        if kind == END {
            return None;
        }
        let length = u32_at(record, at + 4)? as usize;
        let bytes = record.get(at..at.checked_add(length)?)?;
        if length < 0x18 {
            return None;
        }
        at += length;
        Some(Attribute {
            kind,
            non_resident: bytes[8] != 0,
            name_length: bytes[9],
            flags: u16_at(bytes, 0x0C)?,
            bytes,
        })
    })
}

/// Every chunk's names in one list, and what extension records add to
/// their base records' slots in `infos`.
fn merge(
    parsed: Vec<Parsed>,
    infos: &mut RecordTable,
) -> (Vec<Entry>, Vec<String>) {
    // Copied in parallel, a chunk per task, into a list filled first: one
    // thread appending millions of names made every other one wait. Room
    // for the names from extension records too, pushed after: any system
    // drive has some, and the first would copy the whole list to grow it.
    let total = parsed.iter().map(|chunk| chunk.names.len()).sum();
    let extra_total: usize =
        parsed.iter().map(|chunk| chunk.extra_names.len()).sum();
    let mut names = Vec::with_capacity(total + extra_total);
    names.par_extend(rayon::iter::repeat_n(Entry::EMPTY, total));
    let mut pieces = Vec::with_capacity(parsed.len());
    let mut rest = names.as_mut_slice();
    for chunk in &parsed {
        let (piece, tail) = rest.split_at_mut(chunk.names.len());
        pieces.push((piece, chunk.names.as_slice()));
        rest = tail;
    }
    pieces
        .into_par_iter()
        .for_each(|(piece, chunk)| piece.copy_from_slice(chunk));
    let mut extra = Vec::new();
    let mut extra_names = Vec::new();
    let mut texts = Vec::with_capacity(parsed.len());
    for chunk in parsed {
        extra.extend(chunk.extra);
        extra_names.extend(chunk.extra_names);
        texts.push(chunk.text);
    }
    // A name in an extension record left behind by a file whose record
    // has since been reused would list that file under the new one's
    // number.
    for (name, sequence) in extra_names {
        let Some(info) = infos.get_mut(name.child as usize) else {
            continue;
        };
        if info.in_use && info.sequence == sequence {
            info.names = info.names.saturating_add(1);
            names.push(name);
        }
    }
    for (number, sequence, apparent, allocated, tag) in extra {
        let Some(info) = infos.get_mut(number as usize) else {
            continue;
        };
        // An extension record left behind by a file whose record has
        // since been reused.
        if !info.in_use || info.sequence != sequence {
            continue;
        }
        info.allocated = info.allocated.saturating_add(allocated);
        if let Some(apparent) = apparent {
            info.apparent = apparent;
        }
        if !info.has_tag() {
            info.set_tag(tag);
        }
    }
    (names, texts)
}

/// Why a tree was not made: the scan was cancelled, or the tree runs
/// deeper than [`MOST_LEVELS`], or holds more than it can number.
struct Stop;

/// What a whole read of the table found, ready for `flat` to build the
/// tree from.
struct Table<'a> {
    infos: RecordTable,
    /// Sorted by parent; `starts[p]..starts[p + 1]` are `p`'s entries.
    names: Vec<Entry>,
    texts: Vec<String>,
    starts: Vec<u32>,
    options: &'a ScanOptions,
    progress: &'a ScanProgress,
    /// Files charged already, when hardlinks count once: totals are
    /// settled as the tree is built, rather than in a pass over it after.
    seen: Option<Seen>,
}

impl<'a> Table<'a> {
    fn of(
        state: State,
        starts: Vec<u32>,
        options: &'a ScanOptions,
        progress: &'a ScanProgress,
    ) -> Self {
        let State {
            infos,
            names,
            texts,
        } = state;
        Self {
            infos,
            names,
            texts,
            starts,
            options,
            progress,
            seen: options.dedup_hardlinks.then(Seen::new),
        }
    }

    fn into_parts(self) -> (State, Vec<u32>) {
        let state = State {
            infos: self.infos,
            names: self.names,
            texts: self.texts,
        };
        (state, self.starts)
    }

    fn name(&self, entry: &Entry) -> &str {
        let at = entry.at as usize;
        self.texts
            .get(entry.chunk as usize)
            .and_then(|text| text.get(at..at + usize::from(entry.len)))
            .unwrap_or_default()
    }

    fn entries(&self, parent: u32) -> &[Entry] {
        let index = parent as usize;
        match (self.starts.get(index), self.starts.get(index + 1)) {
            (Some(&start), Some(&end)) => {
                &self.names[start as usize..end as usize]
            }
            _ => &[],
        }
    }
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

fn unsigned(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .rev()
        .fold(0, |value, &byte| (value << 8) | u64::from(byte))
}

/// A little-endian two's complement number of any width up to eight bytes.
fn signed(bytes: &[u8]) -> i64 {
    let value = unsigned(bytes);
    let bits = bytes.len() * 8;
    if bits == 0 || bits >= 64 {
        return value.cast_signed();
    }
    let shift = 64 - bits;
    (value << shift).cast_signed() >> shift
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;

    use super::*;
    use crate::tree::{Node, NodeKind};

    /// The tree a table makes, the way a scan makes it.
    fn tree_of(table: &Table<'_>) -> Result<Tree, Stop> {
        let mut tree = table.build()?;
        crate::classify::classify(&mut tree);
        Ok(tree)
    }

    const RECORD: usize = 1024;
    const USA_AT: usize = 0x30;
    const CHECK: [u8; 2] = [0xAB, 0xCD];

    fn resident(kind: u32, value: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; 0x18];
        bytes[0..4].copy_from_slice(&kind.to_le_bytes());
        bytes[0x10..0x14].copy_from_slice(&(value.len() as u32).to_le_bytes());
        bytes[0x14..0x16].copy_from_slice(&0x18_u16.to_le_bytes());
        bytes.extend_from_slice(value);
        bytes.resize(bytes.len().next_multiple_of(8), 0);
        let length = bytes.len() as u32;
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        bytes
    }

    fn non_resident(kind: u32, allocated: u64, length: u64) -> Vec<u8> {
        let mut bytes = vec![0; 0x48];
        bytes[0..4].copy_from_slice(&kind.to_le_bytes());
        bytes[4..8].copy_from_slice(&0x48_u32.to_le_bytes());
        bytes[8] = 1;
        bytes[0x20..0x22].copy_from_slice(&0x40_u16.to_le_bytes());
        bytes[0x28..0x30].copy_from_slice(&allocated.to_le_bytes());
        bytes[0x30..0x38].copy_from_slice(&length.to_le_bytes());
        bytes
    }

    fn name(parent: u32, text: &str, namespace: u8) -> Vec<u8> {
        let mut value = vec![0; 0x42];
        value[0..8].copy_from_slice(&u64::from(parent).to_le_bytes());
        let units: Vec<u16> = text.encode_utf16().collect();
        value[0x40] = units.len() as u8;
        value[0x41] = namespace;
        for unit in units {
            value.extend_from_slice(&unit.to_le_bytes());
        }
        resident(FILE_NAME, &value)
    }

    fn standard(attributes: u32) -> Vec<u8> {
        let mut value = vec![0; 0x48];
        value[0x20..0x24].copy_from_slice(&attributes.to_le_bytes());
        resident(STANDARD_INFORMATION, &value)
    }

    /// A record as the disk holds it: sector tails swapped for the check
    /// value, their real bytes kept in the update sequence array.
    fn record(flags: u16, base: u64, attributes: &[Vec<u8>]) -> Vec<u8> {
        let mut bytes = vec![0; RECORD];
        bytes[0..4].copy_from_slice(b"FILE");
        bytes[4..6].copy_from_slice(&(USA_AT as u16).to_le_bytes());
        bytes[6..8].copy_from_slice(&3_u16.to_le_bytes());
        bytes[0x14..0x16].copy_from_slice(&0x38_u16.to_le_bytes());
        bytes[0x16..0x18].copy_from_slice(&flags.to_le_bytes());
        bytes[0x20..0x28].copy_from_slice(&base.to_le_bytes());
        let mut at = 0x38;
        for attribute in attributes {
            bytes[at..at + attribute.len()].copy_from_slice(attribute);
            at += attribute.len();
        }
        bytes[at..at + 4].copy_from_slice(&END.to_le_bytes());
        bytes[USA_AT..USA_AT + 2].copy_from_slice(&CHECK);
        for sector in 1..3 {
            let end = sector * 512;
            let (tail, usa) = (end - 2, USA_AT + 2 * sector);
            bytes.copy_within(tail..end, usa);
            bytes[tail..end].copy_from_slice(&CHECK);
        }
        bytes
    }

    fn parse(bytes: &mut [u8], number: u32) -> (Parsed, Info) {
        let mut out = Parsed::default();
        let mut info = Info::default();
        parse_record(bytes, number, 0, &mut out, &mut info);
        (out, info)
    }

    #[test]
    fn a_record_gives_its_name_size_and_attributes() {
        let mut bytes = record(
            IN_USE,
            0,
            &[
                standard(FILE_ATTRIBUTE_HIDDEN),
                name(40, "REPORT~1.TXT", DOS_NAMESPACE),
                name(40, "report for the board.txt", 1),
                non_resident(DATA, 8192, 5000),
            ],
        );
        let (out, info) = parse(&mut bytes, 77);

        assert!(info.in_use);
        assert!(!info.directory);
        assert!(info.hidden);
        assert_eq!((info.apparent, info.allocated), (5000, 8192));
        // The 8.3 alias is the same entry again, not a second name.
        let [entry] = &out.names[..] else {
            panic!("one name: {}", out.names.len());
        };
        assert_eq!((entry.parent, entry.child), (40, 77));
        let at = entry.at as usize;
        assert_eq!(
            &out.text[at..at + usize::from(entry.len)],
            "report for the board.txt"
        );
        assert_eq!(out.files, 1);
    }

    #[test]
    fn reparse_tag_precedence_preserves_directory_and_link_kinds() {
        const ORDINARY: u32 = 0x8000_0017;
        const SURROGATE: u32 = 0xA000_0003;
        for (reparse_attribute, tag, hint, extra, kind) in [
            (true, ORDINARY, SURROGATE, 0, NodeKind::Directory),
            (true, 0, SURROGATE, 0, NodeKind::Symlink),
            (true, ORDINARY, 0, SURROGATE, NodeKind::Directory),
            (true, 0, 0, SURROGATE, NodeKind::Symlink),
            (false, SURROGATE, 0, 0, NodeKind::Directory),
        ] {
            let mut named = name(ROOT, "target", 1);
            let at = usize::from(u16_at(&named, 0x14).expect("value offset"));
            named[at + 0x38..at + 0x3C]
                .copy_from_slice(&FILE_ATTRIBUTE_REPARSE_POINT.to_le_bytes());
            named[at + 0x3C..at + 0x40].copy_from_slice(&hint.to_le_bytes());
            let mut base = record(
                IN_USE | IS_DIRECTORY,
                0,
                &[
                    standard(if reparse_attribute {
                        FILE_ATTRIBUTE_REPARSE_POINT
                    } else {
                        0
                    }),
                    named,
                    resident(REPARSE_POINT, &tag.to_le_bytes()),
                ],
            );
            base[0x10..0x12].copy_from_slice(&1_u16.to_le_bytes());
            let extension = record(
                IN_USE,
                32 | (1 << 48),
                &[resident(REPARSE_POINT, &extra.to_le_bytes())],
            );
            let state = read_all(&[(32, base), (33, extension)]);
            let info = *state.infos.get(32).expect("base record");
            let options = ScanOptions::default();
            let progress = ScanProgress::default();
            let table =
                table(&[(32, ROOT, 5, "target", info)], &options, &progress);
            let tree = tree_of(&table).ok().expect("tree");
            assert_eq!(tree.root().child(0).expect("entry").kind(), kind);
        }
    }

    #[test]
    fn a_free_or_torn_record_contributes_nothing() {
        let mut free = record(0, 0, &[name(5, "gone", 1)]);
        assert!(parse(&mut free, 20).0.names.is_empty());

        // A write that reached only one sector: its tail no longer holds
        // the check value, and the record cannot be trusted.
        let mut torn = record(IN_USE, 0, &[name(5, "half", 1)]);
        torn[1022] ^= 0xFF;
        let (out, info) = parse(&mut torn, 21);
        assert!(!info.in_use && out.names.is_empty());
    }

    #[test]
    fn a_name_left_in_a_reused_records_extension_is_dropped() {
        // Record 0x5A is now a file at sequence 8. Record 300 is an
        // extension left over from sequence 7; record 301 is current.
        let mut base = record(IN_USE, 0, &[name(5, "current", 1)]);
        base[0x10..0x12].copy_from_slice(&8_u16.to_le_bytes());
        let mut stale = record(IN_USE, 0x5A | (7 << 48), &[name(5, "old", 1)]);
        let mut fresh =
            record(IN_USE, 0x5A | (8 << 48), &[name(5, "second", 1)]);
        let mut out = Parsed::default();
        let mut infos = vec![Info::default(); 400];
        parse_record(&mut base, 0x5A, 0, &mut out, &mut infos[0x5A]);
        parse_record(&mut stale, 300, 0, &mut out, &mut infos[300]);
        parse_record(&mut fresh, 301, 0, &mut out, &mut infos[301]);
        let mut infos = infos.into();

        let (names, texts) = merge(vec![out], &mut infos);
        let mut listed: Vec<&str> = names
            .iter()
            .map(|entry| {
                let at = entry.at as usize;
                &texts[0][at..at + usize::from(entry.len)]
            })
            .collect();
        listed.sort_unstable();
        assert_eq!(listed, ["current", "second"]);
        // Nor is the stale name counted as a link.
        assert_eq!(infos.get(0x5A).expect("base record").names, 2);
    }

    /// A file of `size` bytes named `names`, at sequence `sequence`.
    fn file(sequence: u16, names: &[(u32, &str)], size: u64) -> Vec<u8> {
        let mut attributes: Vec<Vec<u8>> = names
            .iter()
            .map(|&(parent, text)| name(parent, text, 1))
            .collect();
        attributes.push(non_resident(DATA, size.next_multiple_of(4096), size));
        let mut bytes = record(IN_USE, 0, &attributes);
        bytes[0x10..0x12].copy_from_slice(&sequence.to_le_bytes());
        bytes
    }

    /// Every record read the way a whole read of the table does.
    fn read_all(records: &[(u32, Vec<u8>)]) -> State {
        let len = records
            .iter()
            .map(|(number, _)| *number as usize + 1)
            .max()
            .unwrap_or(0);
        let mut infos = vec![Info::default(); len];
        let mut out = Parsed::default();
        for (number, bytes) in records {
            let mut bytes = bytes.clone();
            let slot = &mut infos[*number as usize];
            parse_record(&mut bytes, *number, 0, &mut out, slot);
        }
        let mut infos = infos.into();
        let (mut names, texts) = merge(vec![out], &mut infos);
        names.sort_by_key(|entry| entry.parent);
        State {
            infos,
            names,
            texts,
        }
    }

    /// `(parent, child, name, size)` for every name, in order.
    fn listing(state: &State) -> Vec<(u32, u32, String, u64)> {
        let mut listed: Vec<_> = state
            .names
            .iter()
            .map(|entry| {
                let at = entry.at as usize;
                let text = &state.texts[entry.chunk as usize]
                    [at..at + usize::from(entry.len)];
                let info = state.infos.get(entry.child as usize).expect("file");
                (entry.parent, entry.child, text.to_owned(), info.apparent)
            })
            .collect();
        listed.sort();
        listed
    }

    #[test]
    fn re_reading_the_changed_files_gives_what_a_whole_read_would() {
        let dir = |sequence: u16| {
            let mut bytes =
                record(IN_USE | IS_DIRECTORY, 0, &[name(5, "dir", 1)]);
            bytes[0x10..0x12].copy_from_slice(&sequence.to_le_bytes());
            bytes
        };
        let before = [
            (30, dir(2)),
            (31, file(1, &[(30, "a.txt")], 100)),
            (32, file(1, &[(30, "b.txt"), (5, "b-link.txt")], 200)),
            (33, file(1, &[(5, "gone.txt")], 300)),
            (40, file(1, &[(30, "kept.txt")], 400)),
        ];
        // 31 is renamed and grows, 32 loses a link, 33 is deleted and
        // a record in a previously absent page is made; 40 is untouched.
        let after = [
            (30, dir(2)),
            (31, file(1, &[(30, "a2.txt")], 150)),
            (32, file(1, &[(30, "b.txt")], 200)),
            (1024, file(1, &[(30, "new.txt")], 50)),
            (40, file(1, &[(30, "kept.txt")], 400)),
        ];
        let mut state = read_all(&before);
        // Re-read as NTFS hands records over, update sequence undone, in
        // two lists as two threads would.
        let changed = [31, 32, 33, 1024];
        let lists = changed
            .chunks(2)
            .enumerate()
            .map(|(chunk, numbers)| {
                let mut out = Parsed::default();
                let mut bases = Vec::new();
                for number in numbers {
                    let Some((_, bytes)) =
                        after.iter().find(|(n, _)| n == number)
                    else {
                        continue;
                    };
                    let mut bytes = bytes.clone();
                    fixup(&mut bytes).expect("fixed up");
                    let mut info = Info::default();
                    parse_fixed(
                        &bytes,
                        *number,
                        chunk as u32,
                        &mut out,
                        &mut info,
                    );
                    bases.push((*number, info));
                }
                (bases, out)
            })
            .collect();
        snapshot::apply(&mut state, &changed, lists).expect("applied");

        assert_eq!(listing(&state), listing(&read_all(&after)));
        assert!(state.names.is_sorted_by_key(|entry| entry.parent));
        assert!(!state.infos.get(33).is_some_and(|info| info.in_use));
        assert_eq!(
            state.infos.get(32).expect("file").names,
            1,
            "no longer a hardlink"
        );
    }

    #[test]
    fn a_root_that_is_not_an_in_use_directory_is_refused() {
        let mut infos = vec![Info::default(); 8];
        assert!(!is_root_directory(&infos.clone().into()));
        infos[ROOT as usize] = Info {
            in_use: true,
            ..Info::default()
        };
        assert!(!is_root_directory(&infos.clone().into()));
        infos[ROOT as usize].directory = true;
        assert!(is_root_directory(&infos.clone().into()));
        assert!(!is_root_directory(&infos[..3].to_vec().into()));
    }

    #[test]
    fn an_extension_record_adds_to_its_base_record() {
        let mut bytes =
            record(IN_USE, 0x5A | (7 << 48), &[non_resident(DATA, 4096, 100)]);
        let (out, info) = parse(&mut bytes, 300);
        assert!(!info.in_use);
        assert_eq!(out.extra, [(0x5A, 7, Some(100), 4096, 0)]);
    }

    #[test]
    fn runs_are_relative_and_can_go_backwards() {
        let mut attribute = vec![0; 0x40];
        attribute[0x20..0x22].copy_from_slice(&0x40_u16.to_le_bytes());
        // 16 clusters at 256, then 8 clusters 128 back, at 128.
        attribute.extend_from_slice(&[0x21, 0x10, 0x00, 0x01]);
        attribute.extend_from_slice(&[0x21, 0x08, 0x80, 0xFF, 0x00]);
        assert_eq!(
            decode_runs(&attribute, 4096),
            Some(vec![(256 * 4096, 16 * 4096), (128 * 4096, 8 * 4096)])
        );
    }

    #[test]
    fn reads_cover_used_records_on_cluster_boundaries() {
        let geometry = Geometry {
            cluster: 4096,
            record: RECORD,
            mft_offset: 0,
            volume: 1 << 30,
            serial: 0,
        };
        let record = RECORD as u64;
        let gap = GAP_BYTES / record;
        let count = 4 * gap;
        let runs = [(1 << 20, count * record)];
        let mut bitmap = vec![0_u8; count as usize / 8];
        let mut used = |number: u64| {
            bitmap[number as usize / 8] |= 1 << (number % 8);
        };
        // One free record short of the gap is read through; the gap
        // itself splits the reads.
        let through = 10 + gap;
        let split = through + 1 + gap;
        for number in [5, 10, through, split] {
            used(number);
        }

        let reads = plan_reads(&runs, &bitmap, &geometry, gap);

        let end = (through + 1).next_multiple_of(4);
        let start = split - split % 4;
        assert_eq!(
            reads,
            [
                ((1 << 20) + 4 * record, (end - 4) * record, 4),
                ((1 << 20) + start * record, 4 * record, start),
            ]
        );
    }

    #[test]
    fn each_read_parses_into_the_slots_of_its_own_records() {
        // Reads cross page boundaries and leave missing pages between
        // them. Adjacent reads share a page, but never a record.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("records");
        let mut bytes = Vec::new();
        for number in 0..96_u64 {
            let number = (number / 8) * 512 + 254 + number % 8;
            bytes.extend(record(
                IN_USE,
                0,
                &[
                    name(ROOT, &format!("f{number}"), 1),
                    non_resident(DATA, 4096, number + 100),
                ],
            ));
        }
        std::fs::write(&path, bytes).expect("records written");
        let reads: Vec<_> = (0..24_u64)
            .filter(|read| read % 3 != 1)
            .map(|read| {
                (read * 4096, 4096, (read / 2) * 512 + 254 + (read % 2) * 4)
            })
            .collect();
        let mut infos = super::table(&reads, RECORD);
        let progress = ScanProgress::default();
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(3)
            .build()
            .expect("a pool");
        let parsed = pool
            .install(|| {
                read_records(
                    path.to_str().expect("a UTF-8 path"),
                    RECORD,
                    &reads,
                    &mut infos,
                    false,
                    &progress,
                )
            })
            .expect("the file reads");
        let (names, texts) = merge(parsed, &mut infos);

        for number in 0..infos.len() {
            let info = infos.get(number).copied().unwrap_or_default();
            let relative = number.checked_sub(254);
            let read = relative.is_some_and(|relative| {
                relative % 512 < 8
                    && ((relative / 512) * 2 + relative % 512 / 4) % 3 != 1
            });
            assert_eq!(info.in_use, read, "record {number}");
            if read {
                assert_eq!(info.apparent, number as u64 + 100);
            }
        }
        assert_eq!(names.len(), 64);
        for entry in &names {
            let at = entry.at as usize;
            let text =
                &texts[entry.chunk as usize][at..][..usize::from(entry.len)];
            assert_eq!(text, format!("f{}", entry.child));
        }
        assert_eq!(progress.snapshot().files, 64);
    }

    #[test]
    fn a_malformed_update_sequence_is_refused_without_panicking() {
        let valid = record(IN_USE, 0, &[]);
        assert!(fixup(&mut valid.clone()).is_some());
        // Counts that do not match 512-byte strides, one of which once
        // made the stride under two bytes, and an array past the record.
        for (count, at) in [(0, USA_AT), (2, USA_AT), (600, USA_AT), (3, 1020)]
        {
            let mut bytes = valid.clone();
            bytes[4..6].copy_from_slice(&(at as u16).to_le_bytes());
            bytes[6..8].copy_from_slice(&(count as u16).to_le_bytes());
            assert!(fixup(&mut bytes).is_none(), "count {count} at {at}");
        }
    }

    #[test]
    fn a_boot_sector_with_impossible_sizes_is_refused() {
        let boot = |sector: u16, per_cluster: u8, per_record: u8| {
            let mut bytes = vec![0; 512];
            bytes[3..11].copy_from_slice(b"NTFS    ");
            bytes[0x0B..0x0D].copy_from_slice(&sector.to_le_bytes());
            bytes[0x0D] = per_cluster;
            bytes[0x30..0x38].copy_from_slice(&4_u64.to_le_bytes());
            bytes[0x40] = per_record;
            bytes
        };
        // 512-byte sectors, 8 to a cluster, 1 KiB records (2^-(-10)).
        let geometry = geometry_of(&boot(512, 8, 0xF6)).expect("valid");
        assert_eq!(
            (geometry.cluster, geometry.record, geometry.mft_offset),
            (4096, 1024, 4 * 4096)
        );
        for (sector, per_cluster, per_record) in [
            (513, 8, 0xF6),    // sector not a power of two
            (512, 0, 0xF6),    // no sectors to a cluster
            (512, 0x90, 0xF6), // a shift past 64 bits
            (512, 0xF0, 0xF6), // a 32 MiB cluster
            (512, 8, 0xBA),    // a record of 2^70 bytes
            (512, 8, 2),       // an 8 KiB record
        ] {
            assert!(
                geometry_of(&boot(sector, per_cluster, per_record)).is_none(),
                "{sector} {per_cluster:#x} {per_record:#x}"
            );
        }
        assert!(geometry_of(&[0; 16]).is_none());
    }

    #[test]
    fn a_name_that_would_resolve_elsewhere_is_skipped() {
        for text in ["", ".", "..", "a/b", "a\\b", "a\0b"] {
            let mut bytes = record(IN_USE, 0, &[name(40, text, 1)]);
            let (out, _) = parse(&mut bytes, 77);
            assert!(out.names.is_empty() && out.text.is_empty(), "{text:?}");
        }
    }

    /// A table of `(record, parent, parent sequence, name, info)` rows
    /// beneath the root, record 5 at sequence 5.
    fn table<'a>(
        rows: &[(u32, u32, u16, &str, Info)],
        options: &'a ScanOptions,
        progress: &'a ScanProgress,
    ) -> Table<'a> {
        let mut infos = vec![Info::default(); 64];
        infos[ROOT as usize] = Info {
            in_use: true,
            directory: true,
            sequence: 5,
            ..Info::default()
        };
        let mut text = String::new();
        let mut names = Vec::new();
        for &(child, parent, parent_sequence, name, info) in rows {
            infos[child as usize] = info;
            names.push(Entry {
                parent,
                parent_sequence,
                child,
                chunk: 0,
                at: text.len() as u32,
                len: name.len() as u16,
            });
            text.push_str(name);
        }
        names.sort_by_key(|entry| entry.parent);
        let starts = starts(&names, infos.len());
        Table {
            infos: infos.into(),
            names,
            texts: vec![text],
            starts,
            options,
            progress,
            seen: options.dedup_hardlinks.then(Seen::new),
        }
    }

    fn paths(node: Node<'_>, prefix: &str, out: &mut Vec<(String, NodeKind)>) {
        for child in node.children() {
            let path = format!("{prefix}{}", child.name());
            out.push((path.clone(), child.kind()));
            paths(child, &format!("{path}/"), out);
        }
    }

    #[test]
    fn the_table_keeps_what_the_walk_would_list() {
        let file = Info {
            in_use: true,
            sequence: 1,
            ..Info::default()
        };
        let dir = Info {
            in_use: true,
            directory: true,
            sequence: 3,
            ..file
        };
        let rows = [
            (20, 5, 5, "visible.txt", file),
            (21, 5, 5, ".dotfile", file),
            (
                22,
                5,
                5,
                "hidden.txt",
                Info {
                    hidden: true,
                    ..file
                },
            ),
            (
                23,
                5,
                5,
                "evicted",
                Info {
                    evicted: true,
                    ..dir
                },
            ),
            // A mount point: a name surrogate, measured as a link.
            (
                24,
                5,
                5,
                "link",
                Info {
                    reparse: Info::REPARSE | Info::TAGGED | Info::SURROGATE,
                    ..dir
                },
            ),
            (25, 5, 5, "sub", dir),
            (26, 25, 3, "inner.txt", file),
            (27, 25, 3, "deeper", dir),
            (28, 27, 3, "deepest.txt", file),
            // The file system's own records.
            (10, 5, 5, "$system", file),
            // Entries of an earlier directory in a reused record.
            (29, 5, 4, "stale.txt", file),
            (30, 25, 2, "stale-in-sub.txt", file),
            (31, 5, 5, "linked.txt", Info { names: 2, ..file }),
            (31, 25, 3, "linked.txt", Info { names: 2, ..file }),
        ];
        let everything = vec![
            ".dotfile",
            "hidden.txt",
            "link",
            "linked.txt",
            "sub",
            "sub/deeper",
            "sub/deeper/deepest.txt",
            "sub/inner.txt",
            "sub/linked.txt",
            "visible.txt",
        ];
        let shallow_visible = vec![
            "link",
            "linked.txt",
            "sub",
            "sub/inner.txt",
            "sub/linked.txt",
            "visible.txt",
        ];
        let progress = ScanProgress::default();
        for (include_hidden, max_depth, expected) in
            [(true, None, everything), (false, Some(1), shallow_visible)]
        {
            let options = ScanOptions {
                include_hidden,
                max_depth,
                ..ScanOptions::default()
            };
            let Ok(root) = tree_of(&table(&rows, &options, &progress)) else {
                panic!("the table is not cut short");
            };
            let mut found = Vec::new();
            paths(root.root(), "", &mut found);
            found.sort_by(|left, right| left.0.cmp(&right.0));
            let kind = |path: &str| match path {
                "link" => NodeKind::Symlink,
                "sub" | "sub/deeper" => NodeKind::Directory,
                _ => NodeKind::File,
            };
            let expected: Vec<(String, NodeKind)> = expected
                .into_iter()
                .map(|path| (path.to_owned(), kind(path)))
                .collect();
            assert_eq!(found, expected, "include_hidden {include_hidden}");

            // Only a file with a second name needs its identity kept: its
            // record, and the sequence number the record had.
            let inode = |name: &str| {
                root.root().child_named(name).and_then(Node::inode)
            };
            assert_eq!(inode("linked.txt"), Some((0, flat::reference(31, 1))));
            assert_eq!(inode("visible.txt"), None);
        }
    }

    #[test]
    fn a_directory_beneath_itself_gives_up_rather_than_recurse() {
        let dir = Info {
            in_use: true,
            directory: true,
            sequence: 1,
            ..Info::default()
        };
        let rows = [
            (40, 5, 5, "loop", dir),
            (41, 40, 1, "inner", dir),
            (40, 41, 1, "again", dir),
        ];
        let options = ScanOptions::default();
        let progress = ScanProgress::default();
        let table = table(&rows, &options, &progress);
        assert!(tree_of(&table).is_err());
        assert!(!progress.is_cancelled());
    }
}

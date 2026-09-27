//! Starting a scan from the last one: the tree it finished, kept on disk,
//! brought up to date from the volume's change journal, the way
//! `Everything` stays current. The journal names every file changed since,
//! and only those records are read again, from NTFS itself rather than the
//! disk, so they are as current as the file system is; `flat.rs` takes
//! them into the tree.
//!
//! Anything that does not line up gives up and reads the whole table: no
//! journal, another journal (it was deleted and made again), a journal
//! that has since dropped the changes wanted (it keeps a few hours of a
//! busy disk), a kept tree of another format, volume, record size or scan
//! options, one that fails its checksum or is not a tree, more changes
//! than a whole read would cost, or changes the kept tree cannot take in.
//!
//! Three kinds of change the journal alone would miss are handled apart. A
//! file still open for writing grows with no new journal entry until it is
//! closed, so a file whose last entry is not a close is read again on
//! every scan until one is. One opened before the journal's oldest entry
//! is not known to be open at all, so the largest files are looked at on
//! every scan. And the disk lags NTFS by the records it has not yet
//! written back: after a whole read, the directories changed in the
//! minutes before it are read again at once, so their entries find them,
//! and the files on the next scan.

use std::fs::File;
use std::io::{self, Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use rustc_hash::FxHashMap;
use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows_sys::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_FILE_RECORD, FSCTL_QUERY_USN_JOURNAL,
    FSCTL_READ_USN_JOURNAL, USN_REASON_CLOSE, USN_REASON_FILE_CREATE,
};

use super::flat::Fresh;
use super::{
    ATTRIBUTE_LIST, Entry, Geometry, Info, Parsed, REFERENCE, ReadExact as _,
    RecordTable, attributes, contents, merge, open_volume, parse_fixed, u16_at,
    u32_at, u64_at,
};
use crate::scan::ScanOptions;
use crate::tree::{Dir, Item, Metric, NONE, Seg, Tree};
use crate::windows::{control, sizes_by_id};

/// Bumped whenever what a kept tree holds changes, so one kept by an
/// older build is read again rather than trusted.
const MAGIC: [u8; 8] = *b"dttree\x00\x03";

/// Changed files past which reading them one by one costs more than
/// reading the whole table: 64,000 took 1 s from NTFS with their records
/// in memory, and three times that without.
const MOST_CHANGES: usize = 100_000;

/// How long before a whole read a change may still be only in memory: its
/// file is read again from NTFS on the next scan. Of 64,000 files the
/// journal named on a busy `C:\`, the disk and NTFS disagreed on 5 changed
/// in the last 5 s, one 48 s old, and none older.
const LAG_TICKS: i64 = 2 * 60 * 10_000_000;

/// How old the last whole read may be before a scan reads the table whole
/// again rather than resume: a bound on what the journal cannot show.
const MOST_AGE_TICKS: i64 = 24 * 60 * 60 * 10_000_000;

/// Files looked at again on every scan, largest first. One held open and
/// growing for hours (a virtual machine's disk, a log) adds no journal
/// entry until it is closed, and one opened before the journal's oldest
/// entry is not known to be open at all: a WSL disk image grew 1 GB in an
/// hour with none. Each is opened by its number for its sizes, which NTFS
/// has current, and only one whose sizes moved is read again: reading all
/// their records took 264-277 ms of every resumed scan, most of it
/// waiting on the extension records and attribute lists of big, broken-up
/// files.
const BIG_FILES: usize = 1024;

/// Journal since the snapshot, in bytes, and files read again, past which
/// a resumed scan writes its state down again. Below both, the next scan
/// replays from the same older snapshot: writing one is 350 MB and more
/// CPU than the rest of a resumed scan, while replaying an hour of a busy
/// `C:\` (about 8 MiB of journal) again costs little.
const RESAVE_JOURNAL: u64 = 8 << 20;
const RESAVE_FILES: usize = 20_000;

/// Bytes of the journal read per call.
const JOURNAL_BUFFER: usize = 1 << 20;

/// The volume's change journal: which one, and the numbers (`USN`s) of its
/// first and next entries.
#[derive(Clone, Copy)]
pub(super) struct Journal {
    id: u64,
    first: u64,
    next: u64,
}

/// Where the next scan picks up the journal, and the files it reads again
/// whatever the journal says.
pub(super) struct Checkpoint {
    journal: u64,
    next: u64,
    /// When the table was last read whole, as a `FILETIME`.
    whole: i64,
    /// Files last seen open for writing: read again until closed.
    open: Vec<u32>,
    /// `open`, and after a whole read the files changed just before it.
    revisit: Vec<u32>,
    /// The largest files, `(record, sequence, size shown)`: read again
    /// when their sizes moved.
    big: Vec<(u32, u16, u64)>,
}

/// What one read of the table found, as `merge` leaves it.
pub(super) struct State {
    pub infos: RecordTable,
    pub names: Vec<Entry>,
    pub texts: Vec<String>,
}

/// The file kept for `letter` in `dir`.
pub(super) fn file(dir: &Path, letter: char) -> PathBuf {
    crate::windows::cache_path(dir, &format!("mft-{letter}.bin"))
}

pub(super) fn query(volume: &File) -> Option<Journal> {
    let mut out = [0_u8; 64];
    let len = control(volume, FSCTL_QUERY_USN_JOURNAL, &[], &mut out).ok()?;
    let out = out.get(..len)?;
    Some(Journal {
        id: u64_at(out, 0)?,
        first: u64_at(out, 8)?,
        next: u64_at(out, 16)?,
    })
}

/// What the journal says of a file since some point: when its last entry
/// was, whether that closed it, whether any made it (a directory made
/// since holds only what the journal names), and whether it is a
/// directory.
#[derive(Clone, Copy, Default)]
struct Change {
    closed: bool,
    stamp: i64,
    created: bool,
    directory: bool,
}

/// Every file the journal names from `from` on, and where it ended.
/// `None` when the journal cannot be read from there: it has dropped those
/// entries, or holds a kind this does not know.
fn changes(
    volume: &File,
    journal: Journal,
    from: u64,
) -> Option<(FxHashMap<u32, Change>, u64)> {
    let mut files = FxHashMap::default();
    let mut buffer = vec![0_u8; JOURNAL_BUFFER];
    let mut at_usn = from;
    loop {
        // READ_USN_JOURNAL_DATA_V0: start, reasons, only on close, timeout,
        // bytes to wait for, journal id.
        let mut input = [0_u8; 40];
        input[0..8].copy_from_slice(&at_usn.to_le_bytes());
        input[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        input[32..40].copy_from_slice(&journal.id.to_le_bytes());
        let len = control(volume, FSCTL_READ_USN_JOURNAL, &input, &mut buffer)
            .ok()?;
        let out = buffer.get(..len)?;
        let next = u64_at(out, 0)?;
        let mut at = 8;
        while at < out.len() {
            let length = u32_at(out, at)? as usize;
            // Version 2 records, which a version 0 request gets on NTFS:
            // 64-bit file references at 8, stamp at 32, reasons at 40,
            // attributes at 52.
            if length < 60 || u16_at(out, at + 4)? != 2 {
                return None;
            }
            let record = out.get(at..at + length)?;
            let number = u32::try_from(u64_at(record, 8)? & REFERENCE).ok()?;
            let reason = u32_at(record, 40)?;
            let change: &mut Change = files.entry(number).or_default();
            change.closed = reason & USN_REASON_CLOSE != 0;
            change.stamp = u64_at(record, 32)?.cast_signed();
            change.created |= reason & USN_REASON_FILE_CREATE != 0;
            change.directory =
                u32_at(record, 52)? & FILE_ATTRIBUTE_DIRECTORY != 0;
            at += length;
        }
        if out.len() <= 8 || next <= at_usn {
            return Some((files, next));
        }
        at_usn = next;
    }
}

/// Now as a `FILETIME`: 100 ns ticks since 1601.
fn now_ticks() -> i64 {
    const EPOCH_TICKS: i64 = 116_444_736_000_000_000;
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos() / 100);
    i64::try_from(since).map_or(i64::MAX, |ticks| ticks + EPOCH_TICKS)
}

/// The checkpoint after a whole read that started at `journal.next`: read
/// on its own thread while the table is. With it, the directories changed
/// just before the read, which the disk may not have yet.
pub(super) fn after_whole_read(
    volume: &File,
    journal: Journal,
) -> Option<(Checkpoint, Vec<u32>)> {
    let started = now_ticks();
    let (files, _) = changes(volume, journal, journal.first)?;
    let mut open = Vec::new();
    let mut revisit = Vec::new();
    let mut directories = Vec::new();
    for (&number, change) in &files {
        if !change.closed {
            open.push(number);
        }
        if !change.closed || change.stamp >= started - LAG_TICKS {
            revisit.push(number);
            if change.directory {
                directories.push(number);
            }
        }
    }
    directories.sort_unstable();
    let checkpoint = Checkpoint {
        journal: journal.id,
        next: journal.next,
        whole: started,
        open,
        revisit,
        big: Vec::new(),
    };
    Some((checkpoint, directories))
}

/// The last scan's tree brought up to date; `None` when a whole read is
/// needed instead. With it, where the next scan picks up, when that is
/// worth writing down: see [`RESAVE_JOURNAL`].
pub(super) fn resume(
    path: &str,
    volume: &File,
    geometry: &Geometry,
    journal: Journal,
    file: &Path,
    options: &ScanOptions,
) -> Option<(Tree, Option<Checkpoint>)> {
    let kept = open(file, geometry, journal, key(options))?;
    let checkpoint = &kept.checkpoint;
    // The tree comes off its file while the journal is read and the files
    // it names are read again from NTFS. Those wait on the file system, on
    // threads of their own: on the scan's, they kept the tree waiting.
    let (flat, journaled) = std::thread::scope(|scope| {
        let journaled = scope.spawn(|| {
            let (files, next) = changes(volume, journal, checkpoint.next)?;
            let mut numbers: Vec<u32> = files.keys().copied().collect();
            numbers.extend(&checkpoint.revisit);
            numbers.sort_unstable();
            numbers.dedup();
            numbers.extend(moved(volume, &checkpoint.big, &numbers, options));
            numbers.sort_unstable();
            if numbers.len() > MOST_CHANGES {
                return None;
            }
            let lists = read_again(path, geometry, &numbers)?;
            Some((files, next, numbers, lists))
        });
        (body(&kept), journaled.join().ok().flatten())
    });
    let (mut flat, (files, next, numbers, lists)) = (flat?, journaled?);
    let fresh = fresh(&lists);
    let created =
        |number| files.get(&number).is_some_and(|change| change.created);
    let touched = flat.patch(&numbers, &fresh, created, options)?;
    crate::classify::classify_where(&mut flat, Some(&touched));
    if next.saturating_sub(checkpoint.next) < RESAVE_JOURNAL
        && numbers.len() < RESAVE_FILES
    {
        return Some((flat, None));
    }
    let mut open: Vec<u32> = checkpoint
        .open
        .iter()
        .copied()
        .filter(|number| !files.contains_key(number))
        .collect();
    open.extend(
        files
            .iter()
            .filter(|(_, change)| !change.closed)
            .map(|(&number, _)| number),
    );
    open.sort_unstable();
    Some((
        flat,
        Some(Checkpoint {
            journal: journal.id,
            next,
            whole: checkpoint.whole,
            revisit: open.clone(),
            open,
            big: Vec::new(),
        }),
    ))
}

/// Those of `big`, the largest files, whose size is not the one kept, or
/// cannot be asked for; `numbers`, sorted, are read again anyway.
fn moved(
    volume: &File,
    big: &[(u32, u16, u64)],
    numbers: &[u32],
    options: &ScanOptions,
) -> Vec<u32> {
    let wanted: Vec<&(u32, u16, u64)> = big
        .iter()
        .filter(|(number, ..)| numbers.binary_search(number).is_err())
        .collect();
    on_threads(&wanted, |_, wanted| {
        Some(
            wanted
                .iter()
                .filter(|&&&(number, sequence, shown)| {
                    let reference =
                        u64::from(number) | u64::from(sequence) << 48;
                    sizes_by_id(volume, reference).ok().is_none_or(
                        |(length, allocated)| {
                            shown
                                != if options.apparent_size {
                                    length
                                } else {
                                    allocated
                                }
                        },
                    )
                })
                .map(|&&(number, ..)| number)
                .collect::<Vec<u32>>(),
        )
    })
    .map_or_else(Vec::new, |lists| lists.into_iter().flatten().collect())
}

/// `work` done on each of as many slices of `all` as the scan has
/// threads, given its place and each on a thread of its own; `None` if any
/// gave none. For work that waits on the file system: requests on one
/// handle opened without overlapped I/O run one at a time, and a scan's
/// worker waiting is one its other work cannot have.
fn on_threads<T: Sync, R: Send>(
    all: &[T],
    work: impl Fn(usize, &[T]) -> Option<R> + Sync,
) -> Option<Vec<R>> {
    if all.is_empty() {
        return Some(Vec::new());
    }
    let threads = rayon::current_num_threads().max(1);
    let per = all.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let work = &work;
        let handles: Vec<_> = all
            .chunks(per)
            .enumerate()
            .map(|(index, slice)| scope.spawn(move || work(index, slice)))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().ok().flatten())
            .collect()
    })
}

/// What re-reading `numbers` found: per list, the base records' facts and
/// the rest parsed, the list's place its chunk number.
type Lists = Vec<(Vec<(u32, Info)>, Parsed)>;

/// Every record of `numbers` as NTFS holds it now.
fn read_again(
    path: &str,
    geometry: &Geometry,
    numbers: &[u32],
) -> Option<Lists> {
    on_threads(numbers, |chunk, numbers| {
        let chunk = u32::try_from(chunk).ok()?;
        let volume = open_volume(path).ok()?;
        let mut out = Parsed::default();
        let mut bases = Vec::with_capacity(numbers.len());
        let mut buffer = Vec::new();
        for &number in numbers {
            if let Some(info) =
                reparse(&volume, geometry, number, chunk, &mut buffer, &mut out)
                    .ok()?
            {
                bases.push((number, info));
            }
        }
        Some((bases, out))
    })
}

/// Replace what `state` holds for each of `numbers` by what its record
/// holds now.
pub(super) fn reread(
    path: &str,
    geometry: &Geometry,
    state: &mut State,
    numbers: &[u32],
) -> Option<()> {
    apply(state, numbers, read_again(path, geometry, numbers)?)
}

/// What each record re-read holds, its extension records' part added as
/// `merge` adds it.
fn fresh(lists: &Lists) -> FxHashMap<u32, Fresh> {
    let mut fresh: FxHashMap<u32, Fresh> = lists
        .iter()
        .flat_map(|(bases, _)| bases)
        .map(|&(number, info)| {
            (
                number,
                Fresh {
                    info,
                    names: Vec::new(),
                },
            )
        })
        .collect();
    let text = |out: &Parsed, entry: &Entry| {
        let at = entry.at as usize;
        out.text
            .get(at..at + usize::from(entry.len))
            .unwrap_or_default()
            .to_owned()
    };
    for (_, out) in lists {
        for entry in &out.names {
            if let Some(record) = fresh.get_mut(&entry.child) {
                let name = text(out, entry);
                record
                    .names
                    .push((entry.parent, entry.parent_sequence, name));
            }
        }
        // A name or size in an extension record left behind by a file
        // whose record has since been reused is not this one's.
        for (entry, sequence) in &out.extra_names {
            if let Some(record) = fresh.get_mut(&entry.child)
                && record.info.sequence == *sequence
            {
                record.info.names = record.info.names.saturating_add(1);
                let name = text(out, entry);
                record
                    .names
                    .push((entry.parent, entry.parent_sequence, name));
            }
        }
        for &(number, sequence, apparent, allocated, tag) in &out.extra {
            let Some(record) = fresh.get_mut(&number) else {
                continue;
            };
            let info = &mut record.info;
            if info.sequence != sequence {
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
    }
    fresh
}

/// Drop everything `state` holds for `numbers`, then add what re-reading
/// them found: per list, the base records' facts and the rest parsed, the
/// list's place its chunk number.
pub(super) fn apply(
    state: &mut State,
    numbers: &[u32],
    lists: Vec<(Vec<(u32, Info)>, Parsed)>,
) -> Option<()> {
    let State {
        infos,
        names,
        texts,
    } = state;
    infos.reserve(
        numbers
            .iter()
            .map(|&number| number as usize..number as usize + 1),
    );
    let mut changed = vec![false; infos.len()];
    for &number in numbers {
        changed[number as usize] = true;
        *infos.get_mut(number as usize)? = Info::default();
    }
    let mut parsed = Vec::with_capacity(lists.len());
    for (bases, out) in lists {
        for (number, info) in bases {
            *infos.get_mut(number as usize)? = info;
        }
        parsed.push(out);
    }
    let first = u32::try_from(texts.len()).ok()?;
    let (mut added, added_texts) = merge(parsed, infos);
    texts.extend(added_texts);
    // The kept names are in parent order, and sorting the few new ones and
    // merging them in keeps it: sorting all again costs a tenth of a
    // second. In place, from the back: a new list of millions of names
    // costs more to fault in than to fill.
    added.sort_unstable_by_key(|name| name.parent);
    names.retain(|name| {
        !changed.get(name.child as usize).copied().unwrap_or(false)
    });
    let mut kept = names.len();
    names.resize(kept + added.len(), Entry::EMPTY);
    for slot in (0..names.len()).rev() {
        let Some(new) = added.last() else { break };
        if kept > 0 && names[kept - 1].parent > new.parent {
            kept -= 1;
            names[slot] = names[kept];
        } else {
            names[slot] = Entry {
                chunk: new.chunk + first,
                ..*new
            };
            added.pop();
        }
    }
    Some(())
}

/// Parse file `number` as NTFS holds it now, its extension records too,
/// into `out`: its base record's facts, or `None` when the record is free
/// or is itself another file's extension, which its base file carries.
fn reparse(
    volume: &File,
    geometry: &Geometry,
    number: u32,
    chunk: u32,
    buffer: &mut Vec<u8>,
    out: &mut Parsed,
) -> io::Result<Option<Info>> {
    let Some(base) = fetch(volume, geometry, number, buffer)? else {
        return Ok(None);
    };
    if u64_at(&base, 0x20).unwrap_or(0) & REFERENCE != 0 {
        return Ok(None);
    }
    let mut info = Info::default();
    parse_fixed(&base, number, chunk, out, &mut info);
    if !info.in_use {
        return Ok(None);
    }
    if attributes(&base).any(|attribute| attribute.kind == ATTRIBUTE_LIST) {
        let list = contents(
            volume,
            geometry,
            std::slice::from_ref(&base),
            ATTRIBUTE_LIST,
        )
        .ok_or_else(|| io::Error::other("unreadable attribute list"))?;
        let mut others = Vec::new();
        let mut at = 0;
        while let (Some(length), Some(reference)) =
            (u16_at(&list, at + 4), u64_at(&list, at + 0x10))
        {
            if let Ok(other) = u32::try_from(reference & REFERENCE)
                && other != number
                && !others.contains(&other)
            {
                others.push(other);
            }
            if length == 0 {
                break;
            }
            at += usize::from(length);
        }
        for other in others {
            let Some(record) = fetch(volume, geometry, other, buffer)? else {
                continue;
            };
            // Only an extension of this file adds to it; `merge` checks its
            // sequence against the base's.
            if u64_at(&record, 0x20).unwrap_or(0) & REFERENCE
                == u64::from(number)
            {
                parse_fixed(&record, other, chunk, out, &mut Info::default());
            }
        }
    }
    Ok(Some(info))
}

/// Record `number` as NTFS holds it now, which the disk may not yet;
/// `None` when it is not in use. NTFS hands it over with its update
/// sequence already undone.
fn fetch(
    volume: &File,
    geometry: &Geometry,
    number: u32,
    buffer: &mut Vec<u8>,
) -> io::Result<Option<Vec<u8>>> {
    // NTFS_FILE_RECORD_OUTPUT_BUFFER: reference, length, record.
    const AT: usize = 12;
    buffer.resize(AT + geometry.record, 0);
    let input = u64::from(number).to_le_bytes();
    let len = control(volume, FSCTL_GET_NTFS_FILE_RECORD, &input, buffer)?;
    let out = buffer.get(..len).unwrap_or_default();
    // A free record gives the nearest in-use one below it instead.
    let (Some(reference), Some(length)) = (u64_at(out, 0), u32_at(out, 8))
    else {
        return Err(io::Error::other("short file record"));
    };
    if reference & REFERENCE != u64::from(number) {
        return Ok(None);
    }
    match out.get(AT..AT + length as usize) {
        Some(record) if record.len() == geometry.record => {
            Ok(Some(record.to_vec()))
        }
        _ => Err(io::Error::other("unexpected file record length")),
    }
}

/// Write the tree `make` gives for the next scan, on a thread of its own,
/// then let go of it: making it and writing it are work the caller need
/// not wait out.
pub(super) fn save_later(
    file: PathBuf,
    geometry: &Geometry,
    options: &ScanOptions,
    checkpoint: Checkpoint,
    make: impl FnOnce() -> Option<Arc<Tree>> + Send + 'static,
) {
    let serial = geometry.serial;
    let record = geometry.record;
    let key = key(options);
    crate::scan::save_later(move || {
        if let Some(tree) = make() {
            let _ = save(&file, serial, record, key, &tree, &checkpoint);
        }
    });
}

/// The options a kept tree was made with, which a scan resuming from it
/// must share: each changes what the tree holds, or its order.
fn key(options: &ScanOptions) -> u64 {
    u64::from(options.apparent_size)
        | u64::from(options.include_hidden) << 1
        | u64::from(options.dedup_hardlinks) << 2
        | u64::from(options.metric == Metric::Files) << 3
}

/// Header: magic, serial, record size, journal, next, when last read
/// whole, the options, and the counts of directories, entries, name bytes,
/// open, revisit and big files, then the checksum of all that follows.
const HEADER: usize = 8 * 14;
const DIR: usize = 46;
const ITEM: usize = 28;
const NUMBER: usize = 4;
const BIG: usize = 16;

/// Bytes per write, and per stretch of the checksum. Two writes of a
/// hundred megabytes and more took a table 427 ms to write; in pieces of a
/// megabyte one took 163-190 ms (`ArenaFable`'s measurement).
const PIECE: usize = 1 << 20;

/// The parts of the file, as the checksum tells them apart.
const DIRS: u64 = 1;
const ITEMS: u64 = 2;
const TEXT: u64 = 3;
const NUMBERS: u64 = 4;
const BIGS: u64 = 5;

fn encode_big(&(record, sequence, size): &(u32, u16, u64), out: &mut [u8]) {
    out[0..4].copy_from_slice(&record.to_le_bytes());
    out[4..6].copy_from_slice(&sequence.to_le_bytes());
    out[8..16].copy_from_slice(&size.to_le_bytes());
}

fn decode_big(bytes: &[u8]) -> (u32, u16, u64) {
    (
        u32_at(bytes, 0).unwrap_or(0),
        u16_at(bytes, 4).unwrap_or(0),
        u64_at(bytes, 8).unwrap_or(0),
    )
}

/// A directory as a kept tree holds it: its runs are one list there, so
/// `first` counts from the list's start, `base` being where its
/// segment's runs begin.
fn encode_dir(dir: &Dir, base: u32, out: &mut [u8]) {
    out[0..8].copy_from_slice(&dir.id.to_le_bytes());
    out[8..12].copy_from_slice(&dir.first.saturating_add(base).to_le_bytes());
    out[12..16].copy_from_slice(&dir.len.to_le_bytes());
    out[16..20].copy_from_slice(&dir.parent.to_le_bytes());
    out[20] = dir.category;
    out[21] = dir.reclaim;
    out[22..30].copy_from_slice(&dir.bytes.to_le_bytes());
    out[30..38].copy_from_slice(&dir.files.to_le_bytes());
    out[38..42].copy_from_slice(&dir.dirs.to_le_bytes());
    out[42..46].copy_from_slice(&dir.modified.to_le_bytes());
}

fn decode_dir(bytes: &[u8]) -> Dir {
    Dir {
        id: u64_at(bytes, 0).unwrap_or(u64::MAX),
        first: u32_at(bytes, 8).unwrap_or(0),
        len: u32_at(bytes, 12).unwrap_or(0),
        parent: u32_at(bytes, 16).unwrap_or(NONE),
        category: bytes.get(20).copied().unwrap_or(0),
        reclaim: bytes.get(21).copied().unwrap_or(0),
        bytes: u64_at(bytes, 22).unwrap_or(0),
        files: u64_at(bytes, 30).unwrap_or(0),
        dirs: u32_at(bytes, 38).unwrap_or(0),
        modified: u32_at(bytes, 42).unwrap_or(0),
        ..Dir::EMPTY
    }
}

/// An entry as a kept tree holds it: its name's place counts from the
/// start of all names, `base` being where its segment's begin.
fn encode_item(item: &Item, base: u32, out: &mut [u8]) {
    out[0..8].copy_from_slice(&item.id.to_le_bytes());
    out[8..12].copy_from_slice(&item.at.saturating_add(base).to_le_bytes());
    out[12..14].copy_from_slice(&item.len.to_le_bytes());
    out[14] = item.kind;
    out[15] = item.flags;
    out[16..24].copy_from_slice(&item.value.to_le_bytes());
    out[24..28].copy_from_slice(&item.modified.to_le_bytes());
}

fn decode_item(bytes: &[u8]) -> Item {
    Item {
        id: u64_at(bytes, 0).unwrap_or(0),
        at: u32_at(bytes, 8).unwrap_or(0),
        len: u16_at(bytes, 12).unwrap_or(0),
        // Not a kind: a tree that holds it is refused.
        kind: bytes.get(14).copied().unwrap_or(u8::MAX),
        flags: bytes.get(15).copied().unwrap_or(0),
        value: u64_at(bytes, 16).unwrap_or(0),
        modified: u32_at(bytes, 24).unwrap_or(0),
        volume: 0,
    }
}

fn save(
    file: &Path,
    serial: u64,
    record: usize,
    key: u64,
    tree: &Tree,
    checkpoint: &Checkpoint,
) -> io::Result<()> {
    // Written whole under another name, then renamed over the old one: a
    // scan stopped halfway leaves the last good tree.
    crate::windows::cache_write(file, |out| {
        write_tree(out, serial, record, key, tree, checkpoint)
    })
}

/// Records written a piece at a time, each piece's checksum as a region
/// read back whole would have it: see [`read_region`].
struct Pieces<'a> {
    out: &'a mut File,
    piece: Vec<u8>,
    /// Bytes a piece holds.
    full: usize,
    region: u64,
    index: usize,
    sum: u64,
}

impl<'a> Pieces<'a> {
    fn new(out: &'a mut File, region: u64, size: usize) -> Self {
        let full = PIECE / size * size;
        Self {
            out,
            piece: Vec::with_capacity(full),
            full,
            region,
            index: 0,
            sum: 0,
        }
    }

    /// Add `bytes`, which must not cross a record's end.
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut bytes = bytes;
        while !bytes.is_empty() {
            let room = self.full - self.piece.len();
            let (now, later) = bytes.split_at(room.min(bytes.len()));
            self.piece.extend_from_slice(now);
            bytes = later;
            if self.piece.len() == self.full {
                self.flush()?;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.sum ^= checksum(self.region, self.index, &self.piece);
        self.out.write_all(&self.piece)?;
        self.index += 1;
        self.piece.clear();
        Ok(())
    }

    fn finish(mut self) -> io::Result<u64> {
        if !self.piece.is_empty() {
            self.flush()?;
        }
        Ok(self.sum)
    }
}

/// The tree, and where the next scan resumes from, into `out`: its
/// segments one after the other, as one list of entries and one of names.
fn write_tree(
    out: &mut File,
    serial: u64,
    record: usize,
    key: u64,
    tree: &Tree,
    checkpoint: &Checkpoint,
) -> io::Result<()> {
    let big = tree.largest(BIG_FILES);
    let numbers: Vec<u32> = checkpoint
        .open
        .iter()
        .chain(&checkpoint.revisit)
        .copied()
        .collect();
    // Where each segment's entries and names begin in the lists written.
    let mut bases = Vec::with_capacity(tree.segs.len());
    let (mut items, mut text) = (0_usize, 0_usize);
    for seg in &tree.segs {
        bases.push((items, text));
        items += seg.items.len();
        text += seg.text.len();
    }
    if [items, text].iter().any(|&count| count > u32::MAX as usize) {
        return Err(io::Error::other("too large to keep"));
    }
    let base = |seg: u32| bases.get(seg as usize).copied().unwrap_or((0, 0));
    // Its whole length first: grown a piece at a time, the file cost the
    // file system an extension per megabyte.
    let length = HEADER
        + tree.dirs.len() * DIR
        + items * ITEM
        + text
        + numbers.len() * NUMBER
        + big.len() * BIG;
    out.set_len(length as u64)?;
    // The header goes in last, once the checksum is known.
    out.write_all(&[0; HEADER])?;
    let mut record_bytes = [0_u8; DIR];
    let mut pieces = Pieces::new(out, DIRS, DIR);
    for dir in &tree.dirs {
        encode_dir(dir, base(dir.seg).0 as u32, &mut record_bytes);
        pieces.write(&record_bytes)?;
    }
    let mut sum = pieces.finish()?;
    let mut pieces = Pieces::new(out, ITEMS, ITEM);
    for (seg, &(_, text)) in tree.segs.iter().zip(&bases) {
        for item in &seg.items {
            encode_item(item, text as u32, &mut record_bytes[..ITEM]);
            pieces.write(&record_bytes[..ITEM])?;
        }
    }
    sum ^= pieces.finish()?;
    let mut pieces = Pieces::new(out, TEXT, 1);
    for seg in &tree.segs {
        pieces.write(seg.text.as_bytes())?;
    }
    sum ^= pieces.finish()?;
    let mut piece = Vec::with_capacity(PIECE);
    sum ^= write_region(
        out,
        &numbers,
        NUMBER,
        NUMBERS,
        |number, out| out.copy_from_slice(&number.to_le_bytes()),
        &mut piece,
    )?;
    sum ^= write_region(out, &big, BIG, BIGS, encode_big, &mut piece)?;
    let mut header = Vec::with_capacity(HEADER);
    header.extend_from_slice(&MAGIC);
    for value in [
        serial,
        record as u64,
        checkpoint.journal,
        checkpoint.next,
        checkpoint.whole.cast_unsigned(),
        key,
        tree.dirs.len() as u64,
        items as u64,
        text as u64,
        checkpoint.open.len() as u64,
        checkpoint.revisit.len() as u64,
        big.len() as u64,
        sum,
    ] {
        header.extend_from_slice(&value.to_le_bytes());
    }
    out.seek(SeekFrom::Start(0))?;
    out.write_all(&header)?;
    Ok(())
}

/// Write `records`, `size` bytes each, a piece at a time through `piece`;
/// returns their checksum.
fn write_region<T: Sync>(
    out: &mut File,
    records: &[T],
    size: usize,
    region: u64,
    encode: impl Fn(&T, &mut [u8]) + Sync,
    piece: &mut Vec<u8>,
) -> io::Result<u64> {
    let mut sum = 0;
    for (index, records) in records.chunks(PIECE / size).enumerate() {
        piece.resize(records.len() * size, 0);
        piece
            .par_chunks_exact_mut(size)
            .zip(records.par_iter())
            .for_each(|(out, record)| encode(record, out));
        sum ^= checksum(region, index, piece);
        out.write_all(piece)?;
    }
    Ok(sum)
}

/// The tree kept in `file`, if it is of this volume and these options and
/// `journal` still reaches back to where it left off.
#[cfg(test)]
fn load(
    file: &Path,
    geometry: &Geometry,
    journal: Journal,
    key: u64,
) -> Option<(Tree, Checkpoint)> {
    let kept = open(file, geometry, journal, key)?;
    let tree = body(&kept)?;
    Some((tree, kept.checkpoint))
}

/// A kept tree's header and lists of files, read and checked before its
/// body.
struct Kept {
    input: File,
    dirs: usize,
    items: usize,
    text: usize,
    /// The checksum the whole file must have, and what its lists of files
    /// make of it.
    sum: u64,
    numbers: u64,
    checkpoint: Checkpoint,
}

/// The header and lists of files of the tree kept in `file`: see [`load`].
fn open(
    file: &Path,
    geometry: &Geometry,
    journal: Journal,
    key: u64,
) -> Option<Kept> {
    let mut input = crate::windows::cache_read(file)?;
    let mut header = [0_u8; HEADER];
    input.read_exact(&mut header).ok()?;
    if header[..8] != MAGIC {
        return None;
    }
    let value = |index: usize| u64_at(&header, 8 * (index + 1));
    let count = |index: usize| usize::try_from(value(index)?).ok();
    if value(0)? != geometry.serial
        || value(1)? != geometry.record as u64
        || value(5)? != key
    {
        return None;
    }
    let next = value(3)?;
    let whole = value(4)?.cast_signed();
    if value(2)? != journal.id
        || next < journal.first
        || next > journal.next
        || now_ticks().saturating_sub(whole) > MOST_AGE_TICKS
    {
        return None;
    }
    let (dirs, items, text) = (count(6)?, count(7)?, count(8)?);
    let (open, revisit, big) = (count(9)?, count(10)?, count(11)?);
    // Sizes checked against the file before anything is allocated for
    // them: a corrupt count must not ask for terabytes.
    let numbers = open.checked_add(revisit)?;
    let body = dirs
        .checked_mul(DIR)?
        .checked_add(items.checked_mul(ITEM)?)?
        .checked_add(text)?;
    let length = HEADER
        .checked_add(body)?
        .checked_add(numbers.checked_mul(NUMBER)?)?
        .checked_add(big.checked_mul(BIG)?)?;
    if length != usize::try_from(input.metadata().ok()?.len()).ok()?
        || [dirs, items, text]
            .iter()
            .any(|&count| count > u32::MAX as usize)
    {
        return None;
    }
    let at = HEADER + body;
    let (numbers, sum) =
        read_region(&input, at, numbers, NUMBER, NUMBERS, |bytes| {
            u32_at(bytes, 0).unwrap_or(0)
        })?;
    let at = at + numbers.len() * NUMBER;
    let (big, big_sum) = read_region(&input, at, big, BIG, BIGS, decode_big)?;
    let (open, revisit) = numbers.split_at(open);
    Some(Kept {
        input,
        dirs,
        items,
        text,
        sum: value(12)?,
        numbers: sum ^ big_sum,
        checkpoint: Checkpoint {
            journal: journal.id,
            next,
            whole,
            open: open.to_vec(),
            revisit: revisit.to_vec(),
            big,
        },
    })
}

/// The tree itself, if it matches its checksum and is a tree: one
/// segment, as the file holds it.
fn body(kept: &Kept) -> Option<Tree> {
    let mut at = HEADER;
    let (dirs, mut found) =
        read_region(&kept.input, at, kept.dirs, DIR, DIRS, decode_dir)?;
    at += kept.dirs * DIR;
    let (items, sum) =
        read_region(&kept.input, at, kept.items, ITEM, ITEMS, decode_item)?;
    found ^= sum;
    at += kept.items * ITEM;
    let mut text = vec![0; kept.text];
    found ^= text
        .par_chunks_mut(PIECE)
        .enumerate()
        .map(|(index, piece)| {
            let offset = (at + index * PIECE) as u64;
            crate::windows::cache_reopen(&kept.input)
                .ok()?
                .seek_read_exact(piece, offset)
                .ok()?;
            Some(checksum(TEXT, index, piece))
        })
        .collect::<Option<Vec<u64>>>()?
        .into_iter()
        .fold(kept.numbers, |sum, piece| sum ^ piece);
    let tree = Tree {
        name: Box::default(),
        dirs,
        segs: vec![Seg {
            items,
            text: String::from_utf8(text).ok()?,
        }],
        volumes: vec![0],
    };
    (found == kept.sum && tree.is_valid()).then_some(tree)
}

/// `count` records of `size` bytes at `offset` in `file`, read a piece at
/// a time on every thread, through a handle of each thread's own, and
/// decoded as they come: no copy of the file's bytes is kept. With their
/// checksum.
fn read_region<T: Clone + Default + Send + Sync>(
    file: &File,
    offset: usize,
    count: usize,
    size: usize,
    region: u64,
    decode: impl Fn(&[u8]) -> T + Sync + Send,
) -> Option<(Vec<T>, u64)> {
    let per = PIECE / size;
    let mut records = Vec::with_capacity(count);
    records.par_extend(rayon::iter::repeat_n(T::default(), count));
    let sums = records
        .par_chunks_mut(per)
        .enumerate()
        .map_init(
            || (crate::windows::cache_reopen(file).ok(), Vec::new()),
            |(input, bytes), (index, records)| {
                bytes.resize(records.len() * size, 0);
                let offset = (offset + index * per * size) as u64;
                input.as_ref()?.seek_read_exact(bytes, offset).ok()?;
                for (record, raw) in
                    records.iter_mut().zip(bytes.chunks_exact(size))
                {
                    *record = decode(raw);
                }
                Some(checksum(region, index, bytes))
            },
        )
        .collect::<Option<Vec<u64>>>()?;
    Some((records, sums.into_iter().fold(0, |sum, piece| sum ^ piece)))
}

/// A check a torn or damaged piece of the file fails. Four lanes of
/// 64-bit words, so the multiplies overlap: a quarter of the time one
/// chain of them took.
fn checksum(region: u64, index: usize, bytes: &[u8]) -> u64 {
    const MIX: u64 = 0x9E37_79B9_7F4A_7C15;
    let seed = (region << 48) ^ index as u64;
    let mut lanes =
        [seed, seed ^ 1, seed ^ 2, seed ^ 3].map(|lane| lane.wrapping_mul(MIX));
    let mut blocks = bytes.chunks_exact(32);
    for block in &mut blocks {
        for (lane, word) in lanes.iter_mut().zip(block.chunks_exact(8)) {
            let word = u64::from_le_bytes(word.try_into().unwrap_or_default());
            *lane = (lane.rotate_left(23) ^ word).wrapping_mul(MIX);
        }
    }
    let mut sum = lanes.iter().fold(bytes.len() as u64, |sum, &lane| {
        (sum.rotate_left(17) ^ lane).wrapping_mul(MIX)
    });
    for &byte in blocks.remainder() {
        sum = (sum.rotate_left(8) ^ u64::from(byte)).wrapping_mul(MIX);
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::super::flat::reference;
    use super::*;
    use crate::tree::{DIRECTORY, FILE, IDENTIFIED};

    const JOURNAL: Journal = Journal {
        id: 0xABCD,
        first: 1000,
        next: 200_000,
    };

    fn geometry(serial: u64) -> Geometry {
        Geometry {
            cluster: 4096,
            record: 1024,
            mft_offset: 0,
            volume: 1 << 30,
            serial,
        }
    }

    /// The root holding `sub` and `résumé.txt`, a file with two names, and
    /// `sub` holding `file.txt`: as a kept tree loads, in one segment.
    fn tree() -> Tree {
        let dir = |record, first, len, parent, bytes| Dir {
            id: reference(record, 1),
            first,
            len,
            parent,
            category: 8,
            reclaim: 0,
            bytes,
            files: len.into(),
            dirs: 1,
            modified: 9,
            ..Dir::EMPTY
        };
        let item = |record, at, len, kind, value| Item {
            id: reference(record, 3),
            at,
            len,
            kind,
            flags: if record == 30 { IDENTIFIED } else { 0 },
            value,
            modified: 7,
            volume: 0,
        };
        Tree {
            name: Box::default(),
            dirs: vec![dir(5, 0, 2, NONE, 12_288), dir(20, 2, 1, 0, 4096)],
            segs: vec![Seg {
                items: vec![
                    item(30, 3, 12, FILE, 8192),
                    item(20, 0, 3, DIRECTORY, 1),
                    item(31, 15, 8, FILE, 4096),
                ],
                text: "subr\u{e9}sum\u{e9}.txtfile.txt".to_owned(),
            }],
            volumes: vec![0],
        }
    }

    /// [`tree`] as a build leaves it: `sub`'s entries in a segment of
    /// their own.
    fn split() -> Tree {
        let mut tree = tree();
        let file = tree.segs[0].items.pop().expect("file.txt");
        tree.segs[0].text.truncate(15);
        tree.segs.push(Seg {
            items: vec![Item { at: 0, ..file }],
            text: "file.txt".to_owned(),
        });
        tree.dirs[1].seg = 1;
        tree.dirs[1].first = 0;
        tree
    }

    #[test]
    fn a_kept_tree_reads_back_as_written_and_a_damaged_one_not_at_all() {
        let options = ScanOptions::default();
        let checkpoint = Checkpoint {
            journal: 0xABCD,
            next: 123_456,
            whole: now_ticks(),
            open: vec![30],
            revisit: vec![31],
            big: Vec::new(),
        };
        let dir = tempfile::TempDir::new().expect("tempdir");
        let file = file(dir.path(), 'C');
        let key = key(&options);
        let saved = split();
        assert!(saved.is_valid());
        save(&file, 7, 1024, key, &saved, &checkpoint).expect("saved");

        let (loaded, kept) =
            load(&file, &geometry(7), JOURNAL, key).expect("loads");
        let expected = tree();
        assert_eq!(loaded.dirs, expected.dirs);
        assert_eq!(loaded.segs.len(), 1);
        assert_eq!(
            (&loaded.segs[0].items, &loaded.segs[0].text),
            (&expected.segs[0].items, &expected.segs[0].text)
        );
        assert_eq!(
            (kept.journal, kept.next, kept.open, kept.revisit, kept.big),
            // The largest files are looked at whatever the journal says.
            (
                0xABCD,
                123_456,
                vec![30],
                vec![31],
                vec![(30, 3, 8192), (31, 3, 4096)]
            )
        );

        // Another volume's, a table of another record size, or a scan
        // with other options is not this one's.
        assert!(load(&file, &geometry(8), JOURNAL, key).is_none());
        let other = Geometry {
            record: 4096,
            ..geometry(7)
        };
        assert!(load(&file, &other, JOURNAL, key).is_none());
        let apparent = ScanOptions {
            apparent_size: true,
            ..ScanOptions::default()
        };
        assert!(
            load(&file, &geometry(7), JOURNAL, super::key(&apparent)).is_none()
        );
        // Nor is one the journal no longer reaches back to, or another
        // journal's.
        let wrapped = Journal {
            first: 200_000,
            ..JOURNAL
        };
        assert!(load(&file, &geometry(7), wrapped, key).is_none());
        let remade = Journal { id: 1, ..JOURNAL };
        assert!(load(&file, &geometry(7), remade, key).is_none());
        // Nor one whose table was last read whole two days ago.
        let old = Checkpoint {
            journal: checkpoint.journal,
            next: checkpoint.next,
            whole: now_ticks() - 2 * MOST_AGE_TICKS,
            open: Vec::new(),
            revisit: Vec::new(),
            big: Vec::new(),
        };
        save(&file, 7, 1024, key, &saved, &old).expect("saved");
        assert!(load(&file, &geometry(7), JOURNAL, key).is_none());
        save(&file, 7, 1024, key, &saved, &checkpoint).expect("saved");

        // A damaged byte fails the checksum; a short file its length.
        let bytes = std::fs::read(&file).expect("read");
        let mut damaged = bytes.clone();
        let middle = damaged.len() / 2 + HEADER / 2;
        damaged[middle] ^= 0x10;
        std::fs::write(&file, &damaged).expect("write");
        assert!(load(&file, &geometry(7), JOURNAL, key).is_none());
        std::fs::write(&file, &bytes[..bytes.len() - 1]).expect("write");
        assert!(load(&file, &geometry(7), JOURNAL, key).is_none());

        // Nor is one that is not a tree: a folder named twice would be
        // built twice, each time for every level such names repeat.
        let mut twice = tree();
        twice.segs[0].items[0] = twice.segs[0].items[1];
        assert!(!twice.is_valid());
        save(&file, 7, 1024, key, &twice, &checkpoint).expect("saved");
        assert!(load(&file, &geometry(7), JOURNAL, key).is_none());
        let mut elsewhere = tree();
        elsewhere.dirs[1].parent = 1;
        assert!(!elsewhere.is_valid());
    }
}

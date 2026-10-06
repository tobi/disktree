//! The persistent index: what the last walk of an operand found, kept so
//! the next one does less.
//!
//! One snapshot per operand, keyed by the directory's device and inode and
//! by the options that shape a walk (`-L`, `-D`, `-x`, the excludes). A
//! snapshot is the walk's whole tree: every listing in `fts` order and every
//! entry's stat.
//!
//! It is used two ways.
//!
//! * Exact, the default. A directory whose inode, ctime and mtime are what
//!   the snapshot recorded has the same entries, because creating, removing
//!   or renaming an entry changes its directory's ctime and mtime. Its
//!   `readdir` is skipped and the recorded names are stat'ed instead. Every
//!   entry is still stat'ed, because a file can grow without its directory
//!   noticing, so the answer is the one a full walk gives.
//!
//!   A timestamp only moves if the change lands in a later tick than the
//!   one it holds. A listing is therefore trusted only when the directory's
//!   ctime was at least [`RACY`] older than the moment the snapshot's walk
//!   began: any change after the listing was read gets a later ctime. This
//!   is git's "racy clean" rule.
//!
//! * Caught up, with `--max-age`. A snapshot younger than the age asked for
//!   is brought up to date from what changed since, without walking the
//!   rest: see `super::refresh`.
//!
//! The format is private to this version: a snapshot that does not parse
//! exactly is treated as missing.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustc_hash::FxHashMap;
use rustix::io::Errno;

use super::args::{Deref, Options};
use super::walk::{
    Entry, ListError, Listing, Meta, Root, Slot, StatError, Time,
};

const MAGIC: &[u8; 8] = b"DTDUIDX4";

/// How much older than the walk a directory's ctime must be before its
/// listing is trusted. Two seconds covers file systems with one-second
/// timestamps and a clock that ticks between the stat and the read.
const RACY: Duration = Duration::from_secs(2);

/// Snapshots kept at most; the least recently used go first.
const KEEP: usize = 256;

/// Where snapshots live, or `None` when there is nowhere sensible.
pub fn directory() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("DISKTREE_DU_INDEX_DIR") {
        return Some(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let base = if cfg!(target_os = "macos") {
        home.map(|home| home.join("Library/Caches"))
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| home.map(|home| home.join(".cache")))
    };
    base.map(|base| base.join("disktree").join("du"))
}

/// The options that change what a walk records, folded into a name.
fn fingerprint(options: &Options, root: &Meta, spelled: &[u8]) -> u64 {
    // FNV-1a: stable across builds and runs, unlike the std hasher.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |bytes: &[u8]| {
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    };
    feed(&root.dev.to_le_bytes());
    feed(&root.ino.to_le_bytes());
    feed(&[
        match options.deref {
            Deref::Physical => 0,
            Deref::Args => 1,
            Deref::All => 2,
        },
        u8::from(options.one_file_system),
    ]);
    // An exclude matches the path as the operand spells it, so with
    // excludes `./a` and `a` walk differently.
    if !options.excludes.is_empty() {
        feed(&(spelled.len() as u64).to_le_bytes());
        feed(spelled);
    }
    for pattern in options.excludes.patterns() {
        feed(&(pattern.len() as u64).to_le_bytes());
        feed(pattern);
    }
    hash
}

/// Whether an operand's walk can be kept at all. With `-L` the walk is a
/// graph that can loop, which a tree-shaped snapshot cannot hold.
pub const fn indexable(options: &Options) -> bool {
    options.index && !matches!(options.deref, Deref::All)
}

/// Where a change journal stood when a snapshot's walk began: the
/// volume's journal UUID and the event id. See `super::fsevents`.
pub type Position = ([u8; 16], u64);

/// One operand's snapshot, read back.
#[derive(Debug)]
pub struct Snapshot {
    /// When the walk that produced it began.
    pub taken: Time,
    /// The journal's position then, where the volume keeps one.
    #[cfg_attr(
        not(target_os = "macos"),
        allow(dead_code, reason = "only macOS keeps a journal to replay")
    )]
    pub journal: Option<Position>,
    /// When the last full walk it descends from began. A catch-up keeps
    /// this, so `--max-age` bounds how long anything a catch-up cannot see
    /// can go unseen.
    pub walked: Time,
    pub root: Meta,
    pub tree: Arc<Slot>,
}

impl Snapshot {
    /// The file a snapshot of `root` under `options` lives in.
    pub fn file(
        dir: &Path,
        options: &Options,
        root: &Meta,
        spelled: &[u8],
    ) -> PathBuf {
        let key = fingerprint(options, root, spelled);
        dir.join(format!("{key:016x}.idx"))
    }

    pub fn load(path: &Path) -> Option<Self> {
        let bytes = fs::read(path).ok()?;
        let mut reader = Reader {
            bytes: &bytes,
            at: 0,
        };
        if reader.take(MAGIC.len())? != MAGIC {
            return None;
        }
        let taken = reader.time()?;
        let walked = reader.time()?;
        let journal = match reader.byte()? {
            0 => None,
            1 => {
                let uuid: [u8; 16] = reader.take(16)?.try_into().ok()?;
                Some((uuid, reader.u64()?))
            }
            _ => return None,
        };
        let root = reader.meta(OWN_DEV | OWN_CTIME, 0)?;
        let tree = Arc::new(Slot::new());
        let _ = tree.set(reader.listing(root.dev)?);
        if reader.at != bytes.len() {
            return None;
        }
        Some(Self {
            taken,
            journal,
            walked,
            root,
            tree,
        })
    }

    /// Whether this snapshot is recent enough for `--max-age` to start from.
    pub fn young(&self, max_age: Duration) -> bool {
        SystemTime::now()
            .duration_since(system_time(self.walked))
            .is_ok_and(|age| age <= max_age)
    }

    /// The listings a walk may reuse, by directory identity.
    pub fn previous(&self) -> Previous {
        let mut dirs = FxHashMap::default();
        let taken = self.taken;
        let limit = Time {
            sec: taken.sec.saturating_sub(RACY.as_secs().cast_signed()),
            nsec: taken.nsec,
        };
        let mut stack: Vec<(Meta, Arc<Slot>)> =
            vec![(self.root, Arc::clone(&self.tree))];
        while let Some((meta, slot)) = stack.pop() {
            let Some(listing) = slot.get() else { continue };
            if listing.error.is_none()
                && meta.ctime < limit
                && meta.mtime < limit
            {
                dirs.insert(meta.key(), (meta, Arc::clone(&slot)));
            }
            for entry in &listing.entries {
                if let (Ok(child), Some(dir)) = (entry.meta, &entry.dir) {
                    stack.push((child, Arc::clone(dir)));
                }
            }
        }
        Previous { dirs }
    }
}

/// The listings the exact mode may reuse.
#[derive(Debug, Default)]
pub struct Previous {
    dirs: FxHashMap<(u64, u64), (Meta, Arc<Slot>)>,
}

impl Previous {
    /// The recorded names of `dir`, in order, when it provably has not
    /// changed since they were read.
    pub fn names(&self, dir: Meta) -> Option<Vec<(Box<[u8]>, u64)>> {
        let (then, slot) = self.dirs.get(&dir.key())?;
        if then.ctime != dir.ctime || then.mtime != dir.mtime {
            return None;
        }
        let listing = slot.get()?;
        Some(
            listing
                .entries
                .iter()
                .map(|entry| (entry.name.clone(), entry.d_ino))
                .collect(),
        )
    }
}

/// Distinguishes the temporary files of concurrent writers: `du . $PWD`
/// saves one snapshot twice at once.
static WRITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Write `root`'s walk as a snapshot, replacing the file atomically.
pub fn save(
    dir: &Path,
    options: &Options,
    root: &Root,
    taken: Time,
    walked: Time,
    journal: Option<Position>,
) -> std::io::Result<()> {
    let (Ok(meta), Some(slot)) = (root.meta, &root.dir) else {
        return Ok(());
    };
    let Some(listing) = slot.get() else {
        return Ok(());
    };
    create_private_dir(dir)?;
    let mut out = Vec::with_capacity(1 << 16);
    out.extend_from_slice(MAGIC);
    put_time(&mut out, taken);
    put_time(&mut out, walked);
    match journal {
        None => out.push(0),
        Some((uuid, event)) => {
            out.push(1);
            out.extend_from_slice(&uuid);
            put_u64(&mut out, event);
        }
    }
    put_meta(&mut out, &meta, OWN_DEV | OWN_CTIME);
    put_listing(&mut out, listing, meta.dev);
    let path = Snapshot::file(dir, options, &meta, &root.path);
    let write = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp =
        path.with_extension(format!("tmp{}.{write}", std::process::id()));
    let result = (|| {
        let mut file = private_file(&temp)?;
        file.write_all(&out)?;
        file.sync_data()?;
        fs::rename(&temp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Drop the least recently written snapshots beyond [`KEEP`].
pub fn prune(dir: &Path) {
    let Ok(read) = fs::read_dir(dir) else { return };
    let mut files: Vec<(SystemTime, PathBuf)> = read
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.path().extension().is_some_and(|ext| ext == "idx")
        })
        .filter_map(|entry| {
            let modified = entry.metadata().and_then(|m| m.modified()).ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    if files.len() <= KEEP {
        return;
    }
    files.sort_by_key(|file| std::cmp::Reverse(file.0));
    for (_, path) in files.into_iter().skip(KEEP) {
        let _ = fs::remove_file(path);
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    // File names are private: the index is readable by its owner only.
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

fn private_file(path: &Path) -> std::io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    fs::File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

pub fn now() -> Time {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Time {
        sec: i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        nsec: since.subsec_nanos(),
    }
}

pub fn system_time(time: Time) -> SystemTime {
    let secs = u64::try_from(time.sec).unwrap_or(0);
    UNIX_EPOCH + Duration::new(secs, time.nsec)
}

// The encoding: LEB128 varints, zigzag for signed values, and a listing as
// its error, its entry count, then each entry followed by its own listing
// when it has one.

fn put_u64(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn put_i64(out: &mut Vec<u8>, value: i64) {
    put_u64(out, ((value << 1) ^ (value >> 63)).cast_unsigned());
}

fn put_time(out: &mut Vec<u8>, time: Time) {
    put_i64(out, time.sec);
    put_u64(out, u64::from(time.nsec));
}

// An entry is a tag byte and then only what the tag says is there. Most
// entries share their directory's device, have the inode `readdir` gave,
// and were last changed when they were last modified, so those three are
// written only when they differ. Access times are not kept: a walk reads
// them afresh, and `--max-age` does not answer `--time=atime`.
const META_OK: u8 = 0;
const META_ERRNO: u8 = 1;
const META_DANGLING: u8 = 2;
const META_MASK: u8 = 3;
const HAS_DIR: u8 = 1 << 2;
const OWN_DEV: u8 = 1 << 3;
const OWN_D_INO: u8 = 1 << 4;
const OWN_CTIME: u8 = 1 << 5;

fn put_meta(out: &mut Vec<u8>, meta: &Meta, tag: u8) {
    if tag & OWN_DEV != 0 {
        put_u64(out, meta.dev);
    }
    put_u64(out, meta.ino);
    put_u64(out, u64::from(meta.mode));
    put_u64(out, meta.nlink);
    put_u64(out, meta.blocks);
    put_i64(out, meta.size);
    put_time(out, meta.mtime);
    if tag & OWN_CTIME != 0 {
        put_time(out, meta.ctime);
    }
}

fn put_errno(out: &mut Vec<u8>, errno: Errno) {
    put_u64(out, u64::from(errno.raw_os_error().unsigned_abs()));
}

/// `dev` is the device of the directory the listing is of.
fn put_listing(out: &mut Vec<u8>, listing: &Listing, dev: u64) {
    match listing.error {
        None => put_u64(out, 0),
        Some(ListError::Unreadable(errno)) => {
            put_u64(out, 1);
            put_errno(out, errno);
        }
        Some(ListError::Partial(errno)) => {
            put_u64(out, 2);
            put_errno(out, errno);
        }
    }
    put_u64(out, listing.entries.len() as u64);
    for entry in &listing.entries {
        let child = entry.dir.as_deref().and_then(OnceLock::get);
        let mut tag = if child.is_some() { HAS_DIR } else { 0 };
        tag |= match entry.meta {
            Ok(meta) => {
                let mut own = META_OK;
                if meta.dev != dev {
                    own |= OWN_DEV;
                }
                if meta.ino != entry.d_ino {
                    own |= OWN_D_INO;
                }
                if meta.ctime != meta.mtime {
                    own |= OWN_CTIME;
                }
                own
            }
            Err(StatError::Errno(_)) => META_ERRNO | OWN_D_INO,
            Err(StatError::Dangling) => META_DANGLING | OWN_D_INO,
        };
        out.push(tag);
        put_u64(out, entry.name.len() as u64);
        out.extend_from_slice(&entry.name);
        if tag & OWN_D_INO != 0 {
            put_u64(out, entry.d_ino);
        }
        match entry.meta {
            Ok(meta) => put_meta(out, &meta, tag),
            Err(StatError::Errno(errno)) => put_errno(out, errno),
            Err(StatError::Dangling) => {}
        }
        if let Some(child) = child {
            let child_dev = entry.meta.map_or(dev, |meta| meta.dev);
            put_listing(out, child, child_dev);
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, len: usize) -> Option<&[u8]> {
        let end = self.at.checked_add(len)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn byte(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u64(&mut self) -> Option<u64> {
        let mut value: u64 = 0;
        for shift in (0..64).step_by(7) {
            let byte = self.byte()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn i64(&mut self) -> Option<i64> {
        let raw = self.u64()?;
        Some((raw >> 1).cast_signed() ^ -(raw & 1).cast_signed())
    }

    fn time(&mut self) -> Option<Time> {
        Some(Time {
            sec: self.i64()?,
            nsec: u32::try_from(self.u64()?).ok()?,
        })
    }

    fn meta(&mut self, tag: u8, dev: u64) -> Option<Meta> {
        let dev = if tag & OWN_DEV != 0 { self.u64()? } else { dev };
        let ino = self.u64()?;
        let mode = u32::try_from(self.u64()?).ok()?;
        let nlink = self.u64()?;
        let blocks = self.u64()?;
        let size = self.i64()?;
        let mtime = self.time()?;
        let ctime = if tag & OWN_CTIME != 0 {
            self.time()?
        } else {
            mtime
        };
        Some(Meta {
            dev,
            ino,
            mode,
            nlink,
            blocks,
            size,
            mtime,
            atime: mtime,
            ctime,
        })
    }

    fn errno(&mut self) -> Option<Errno> {
        Some(Errno::from_raw_os_error(i32::try_from(self.u64()?).ok()?))
    }

    fn listing(&mut self, dev: u64) -> Option<Listing> {
        self.listing_at(dev, 0)
    }

    /// A corrupt file must not become deep recursion or a huge allocation:
    /// nesting is bounded by what a path can hold, and every entry takes at
    /// least three bytes.
    fn listing_at(&mut self, dev: u64, depth: usize) -> Option<Listing> {
        if depth > 4096 {
            return None;
        }
        let error = match self.u64()? {
            0 => None,
            1 => Some(ListError::Unreadable(self.errno()?)),
            2 => Some(ListError::Partial(self.errno()?)),
            _ => return None,
        };
        let count = usize::try_from(self.u64()?).ok()?;
        // A corrupt count must not become a huge allocation.
        let left = self.bytes.len().saturating_sub(self.at);
        if count > left / 3 {
            return None;
        }
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let tag = self.byte()?;
            let len = usize::try_from(self.u64()?).ok()?;
            let name: Box<[u8]> = self.take(len)?.into();
            let d_ino = if tag & OWN_D_INO != 0 {
                Some(self.u64()?)
            } else {
                None
            };
            let meta = match tag & META_MASK {
                META_OK => Ok(self.meta(tag, dev)?),
                META_ERRNO => Err(StatError::Errno(self.errno()?)),
                META_DANGLING => Err(StatError::Dangling),
                _ => return None,
            };
            let d_ino = match (d_ino, meta) {
                (Some(d_ino), _) => d_ino,
                (None, Ok(meta)) => meta.ino,
                (None, Err(_)) => return None,
            };
            let dir = if tag & HAS_DIR != 0 {
                let child_dev = meta.map_or(dev, |meta| meta.dev);
                let slot = Arc::new(Slot::new());
                let _ = slot.set(self.listing_at(child_dev, depth + 1)?);
                Some(slot)
            } else {
                None
            };
            entries.push(Entry {
                name,
                d_ino,
                meta,
                dir,
            });
        }
        Some(Listing { entries, error })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(ino: u64, sec: i64) -> Meta {
        let time = Time { sec, nsec: 5 };
        Meta {
            dev: 7,
            ino,
            mode: 0o040_755,
            nlink: 2,
            blocks: 8,
            size: -1,
            mtime: time,
            atime: time,
            ctime: time,
        }
    }

    #[test]
    fn a_corrupt_count_is_refused_not_allocated() {
        let mut out = Vec::new();
        put_u64(&mut out, 0);
        put_u64(&mut out, u64::from(u32::MAX));
        let mut reader = Reader { bytes: &out, at: 0 };
        assert!(reader.listing(1).is_none());
    }

    #[test]
    fn varints_round_trip() {
        let mut out = Vec::new();
        for value in [0, 1, 127, 128, u64::MAX] {
            put_u64(&mut out, value);
        }
        for value in [0, -1, 1, i64::MIN, i64::MAX] {
            put_i64(&mut out, value);
        }
        let mut reader = Reader { bytes: &out, at: 0 };
        for value in [0, 1, 127, 128, u64::MAX] {
            assert_eq!(reader.u64(), Some(value));
        }
        for value in [0, -1, 1, i64::MIN, i64::MAX] {
            assert_eq!(reader.i64(), Some(value));
        }
    }

    #[test]
    fn listings_round_trip_and_racy_ones_are_not_reused() {
        let child = Listing {
            entries: vec![Entry {
                name: b"f".as_slice().into(),
                d_ino: 3,
                meta: Err(StatError::Errno(Errno::ACCESS)),
                dir: None,
            }],
            ..Listing::default()
        };
        let slot = Arc::new(Slot::new());
        let _ = slot.set(child);
        let listing = Listing {
            entries: vec![
                Entry {
                    name: b"old".as_slice().into(),
                    d_ino: 2,
                    meta: Ok(meta(2, 100)),
                    dir: Some(slot),
                },
                Entry {
                    name: b"recent".as_slice().into(),
                    d_ino: 9,
                    meta: Ok(meta(9, 999)),
                    dir: Some(Arc::new(Slot::new())),
                },
            ],
            ..Listing::default()
        };
        let mut out = Vec::new();
        put_listing(&mut out, &listing, 7);
        let mut reader = Reader { bytes: &out, at: 0 };
        let back = reader.listing(7).expect("parses");
        assert_eq!(reader.at, out.len());
        assert_eq!(back.entries.len(), 2);

        let tree = Arc::new(Slot::new());
        let _ = tree.set(back);
        let snapshot = Snapshot {
            taken: Time { sec: 1000, nsec: 0 },
            journal: None,
            walked: Time { sec: 1000, nsec: 0 },
            root: meta(1, 10),
            tree,
        };
        let previous = snapshot.previous();
        assert_eq!(
            previous.names(meta(2, 100)).map(|names| names.len()),
            Some(1),
            "an old, unchanged directory is reused"
        );
        assert!(
            previous.names(meta(2, 101)).is_none(),
            "a changed one is read again"
        );
        assert!(
            previous.names(meta(9, 999)).is_none(),
            "one changed within the racy window is never trusted"
        );
    }
}

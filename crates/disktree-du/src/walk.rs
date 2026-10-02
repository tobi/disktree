//! The parallel half of du: list every directory and stat every entry, on
//! all cores, keeping each directory's entries in the order GNU's `fts`
//! would visit them. Nothing here decides what counts; hard links, `-x`,
//! `--exclude` and the depth rules are applied afterwards, in order, by
//! [`crate::report`], because which of two links is "first" depends on the
//! order of the walk and a parallel walk has none.
//!
//! The order is `fts`'s: `readdir` order, except that a directory read in a
//! batch of more than 10,000 entries has that batch sorted by inode number
//! (gnulib does this to speed up the stats on most file systems, and skips
//! it on the few where it does not help). Batches are 100,000 entries.

use std::sync::{Arc, Mutex, OnceLock};

use rayon::prelude::*;
use rustc_hash::FxHashMap;
use rustix::fd::{AsFd, BorrowedFd, OwnedFd};
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, openat, statat};
use rustix::io::Errno;

use crate::args::Deref;
use crate::exclude::Excludes;
use crate::index::Previous;

/// `FTS_INODE_SORT_DIR_ENTRIES_THRESHOLD` and `FTS_MAX_READDIR_ENTRIES`.
const INODE_SORT_THRESHOLD: usize = 10_000;
const READDIR_BATCH: usize = 100_000;

/// Directories this large have their stats spread over the pool too; below
/// it a directory is one task, which is cheaper than splitting it.
const PARALLEL_STATS: usize = 512;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Time {
    pub sec: i64,
    pub nsec: u32,
}

/// What du needs from `struct stat`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub dev: u64,
    pub ino: u64,
    pub mode: u32,
    pub nlink: u64,
    pub blocks: u64,
    pub size: i64,
    pub mtime: Time,
    pub atime: Time,
    pub ctime: Time,
}

impl Meta {
    #[allow(
        trivial_numeric_casts,
        clippy::unnecessary_cast,
        clippy::cast_possible_wrap,
        reason = "the field types of struct stat differ by platform"
    )]
    pub fn from_stat(stat: &rustix::fs::Stat) -> Self {
        let time = |sec: i64, nsec: i64| Time {
            sec,
            nsec: nsec.clamp(0, 999_999_999) as u32,
        };
        Self {
            dev: stat.st_dev as u64,
            ino: stat.st_ino as u64,
            mode: stat.st_mode as u32,
            nlink: stat.st_nlink as u64,
            blocks: stat.st_blocks as u64,
            size: stat.st_size as i64,
            mtime: time(stat.st_mtime as i64, stat.st_mtime_nsec as i64),
            atime: time(stat.st_atime as i64, stat.st_atime_nsec as i64),
            ctime: time(stat.st_ctime as i64, stat.st_ctime_nsec as i64),
        }
    }

    #[allow(
        trivial_numeric_casts,
        reason = "the mode is a u16 on some systems and a u32 on others"
    )]
    pub const fn file_type(&self) -> FileType {
        FileType::from_raw_mode(self.mode as rustix::fs::RawMode)
    }

    pub fn is_dir(&self) -> bool {
        self.file_type() == FileType::Directory
    }

    /// Equal in everything a directory's listing or du's numbers depend
    /// on. Access time is left out: reading a directory moves it.
    pub fn unchanged(&self, other: &Self) -> bool {
        Self {
            atime: other.atime,
            ..*self
        } == *other
    }

    pub const fn key(&self) -> (u64, u64) {
        (self.dev, self.ino)
    }
}

/// Why an entry has no stat. `Dangling` is `fts`'s `FTS_SLNONE`: a link
/// followed to nothing, which du reports without an errno.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatError {
    Errno(Errno),
    Dangling,
}

pub type Slot = OnceLock<Listing>;

#[derive(Clone, Debug)]
pub struct Entry {
    pub name: Box<[u8]>,
    pub d_ino: u64,
    pub meta: Result<Meta, StatError>,
    /// The directory's listing, when this entry is one the walk went into.
    pub dir: Option<Arc<Slot>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListError {
    /// `FTS_DNR`: could not be opened, or failed before its first entry.
    Unreadable(Errno),
    /// `FTS_ERR`: failed part way; the entries before the failure stand.
    Partial(Errno),
}

#[derive(Debug, Default)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub error: Option<ListError>,
}

#[derive(Debug)]
pub struct Root {
    pub path: Vec<u8>,
    pub meta: Result<Meta, StatError>,
    pub dir: Option<Arc<Slot>>,
}

/// Whether the bulk attribute listing is in use.
#[cfg(target_os = "macos")]
fn bulk_enabled() -> bool {
    crate::bulk::enabled()
}

#[cfg(not(target_os = "macos"))]
const fn bulk_enabled() -> bool {
    false
}

/// `fts_open` trims a run of trailing slashes on an operand to one, but
/// leaves `//` alone.
pub fn trim_operand(name: &[u8]) -> Vec<u8> {
    let mut len = name.len();
    if len > 2 && name[len - 1] == b'/' {
        while len > 1 && name[len - 2] == b'/' {
            len -= 1;
        }
    }
    name[..len].to_vec()
}

/// A child's path the way `fts` builds it: no second slash after a parent
/// that already ends in one.
pub fn join(parent: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = Vec::with_capacity(parent.len() + name.len() + 1);
    path.extend_from_slice(parent);
    if parent.last() != Some(&b'/') {
        path.push(b'/');
    }
    path.extend_from_slice(name);
    path
}

/// The directories above the one being listed, as a chain each task
/// extends by one: a directory that is its own ancestor (a bind mount of
/// one) is a cycle, which `fts` reports and du skips.
struct Above {
    key: (u64, u64),
    up: Option<Arc<Self>>,
}

fn is_above(mut chain: Option<&Arc<Above>>, key: (u64, u64)) -> bool {
    while let Some(link) = chain {
        if link.key == key {
            return true;
        }
        chain = link.up.as_ref();
    }
    false
}

/// Open a directory by path. A path longer than the system allows in one
/// call is opened a piece at a time, each relative to the last, as `fts`
/// does by keeping a descriptor for every level.
pub fn open_dir(path: &[u8], flags: OFlags) -> Result<OwnedFd, Errno> {
    // Comfortably below every PATH_MAX in use (1024 on macOS).
    const PIECE: usize = 768;
    match openat(CWD, path, flags, Mode::empty()) {
        Err(Errno::NAMETOOLONG) => {}
        other => return other,
    }
    let through = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let (mut at, rest) = match path.strip_prefix(b"/") {
        Some(rest) => (Some(openat(CWD, "/", through, Mode::empty())?), rest),
        None => (None, path),
    };
    let names: Vec<&[u8]> = rest
        .split(|&b| b == b'/')
        .filter(|n| !n.is_empty())
        .collect();
    let mut start = 0;
    while start < names.len() {
        let mut end = start;
        let mut len = 0;
        while end < names.len()
            && (end == start || len + names[end].len() < PIECE)
        {
            len += names[end].len() + 1;
            end += 1;
        }
        let piece = names[start..end].join(&b'/');
        let last = end == names.len();
        let piece_flags = if last { flags } else { through };
        let next = match &at {
            Some(dir) => {
                openat(dir, piece.as_slice(), piece_flags, Mode::empty())
            }
            None => openat(CWD, piece.as_slice(), piece_flags, Mode::empty()),
        }?;
        at = Some(next);
        start = end;
    }
    at.ok_or(Errno::NOENT)
}

/// `stat_at` by path, for paths of any length.
pub fn stat_path(path: &[u8], follow: bool) -> Result<Meta, StatError> {
    match stat_at(CWD, path, follow) {
        Err(StatError::Errno(Errno::NAMETOOLONG)) => {}
        other => return other,
    }
    let at = path.iter().rposition(|&b| b == b'/').unwrap_or(0);
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let dir = open_dir(&path[..at.max(1)], flags).map_err(StatError::Errno)?;
    stat_at(dir.as_fd(), &path[at + 1..], follow)
}

/// What every task of one operand's walk shares.
struct Context<'a> {
    root_dev: u64,
    /// What the index remembers, to skip `readdir` where nothing changed.
    previous: Option<&'a Previous>,
}

pub struct Walker<'a> {
    pub deref: Deref,
    pub one_file_system: bool,
    pub excludes: &'a Excludes,
    /// With `-L` one directory can be reached by many paths, and a cycle
    /// by infinitely many; each is listed once and shared.
    seen: Mutex<FxHashMap<(u64, u64), Arc<Slot>>>,
}

const fn stat_flags(follow: bool) -> AtFlags {
    if follow {
        AtFlags::empty()
    } else {
        AtFlags::SYMLINK_NOFOLLOW
    }
}

/// `stat` or `lstat` relative to `dir`, telling a dangling link from any
/// other failure the way `fts_stat` does.
pub fn stat_at(
    dir: BorrowedFd<'_>,
    name: &[u8],
    follow: bool,
) -> Result<Meta, StatError> {
    match statat(dir, name, stat_flags(follow)) {
        Ok(stat) => Ok(Meta::from_stat(&stat)),
        Err(Errno::NOENT) if follow => {
            match statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(_) => Err(StatError::Dangling),
                Err(_) => Err(StatError::Errno(Errno::NOENT)),
            }
        }
        Err(errno) => Err(StatError::Errno(errno)),
    }
}

#[cfg(target_os = "linux")]
fn inode_sort_useful(dir: BorrowedFd<'_>) -> bool {
    // gnulib skips the sort on CIFS, NFS, tmpfs and Lustre.
    const NO_SORT: [u64; 4] = [0xFF53_4D42, 0x6969, 0x0102_1994, 0x0BD0_0BD0];
    #[allow(
        clippy::unnecessary_cast,
        clippy::cast_sign_loss,
        reason = "f_type is signed on some targets"
    )]
    rustix::fs::fstatfs(dir)
        .map_or(true, |fs| !NO_SORT.contains(&(fs.f_type as u64)))
}

#[cfg(not(target_os = "linux"))]
const fn inode_sort_useful(_dir: BorrowedFd<'_>) -> bool {
    true
}

impl<'a> Walker<'a> {
    pub fn new(
        deref: Deref,
        one_file_system: bool,
        excludes: &'a Excludes,
    ) -> Self {
        Self {
            deref,
            one_file_system,
            excludes,
            seen: Mutex::new(FxHashMap::default()),
        }
    }

    /// Stat one operand, without walking it.
    pub fn root(&self, operand: &[u8]) -> Root {
        let path = trim_operand(operand);
        let follow = self.deref != Deref::Physical;
        let meta = stat_at(CWD, &path, follow);
        Root {
            path,
            meta,
            dir: None,
        }
    }

    /// Whether listings from the index can save this walk any work. With
    /// macOS's bulk attributes a directory costs one call either way.
    pub fn reuses_listings(&self) -> bool {
        !(self.deref != Deref::All && bulk_enabled())
    }

    /// Whether the walk goes into this operand at all. An excluded operand
    /// is never visited, so neither is its content.
    pub fn descends(&self, root: &Root) -> bool {
        root.meta.is_ok_and(|meta| meta.is_dir())
            && !self.excludes.excludes(&root.path)
    }

    /// Walk the operand `root` names, reusing `previous` listings where
    /// the directory provably has not changed.
    pub fn fill(&self, root: &mut Root, previous: Option<&Previous>) {
        let Ok(meta) = root.meta else { return };
        if !self.descends(root) {
            return;
        }
        let slot = Arc::new(Slot::new());
        root.dir = Some(Arc::clone(&slot));
        if self.deref == Deref::All {
            self.seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(meta.key(), Arc::clone(&slot));
        }
        let path = root.path.clone();
        let context = Context {
            root_dev: meta.dev,
            previous,
        };
        rayon::scope(|scope| {
            self.list(scope, &context, &path, meta, true, &slot, None);
        });
    }

    /// Whether the walk goes into `child`, the entry at `path`: a
    /// directory, on the operand's device under `-x`, and not excluded.
    /// With `-L` an excluded directory is still walked, because its listing
    /// may be shared with a path that is not excluded.
    pub fn enters(&self, path: &[u8], child: &Meta, root_dev: u64) -> bool {
        child.is_dir()
            && !(self.one_file_system && child.dev != root_dev)
            && !(self.deref != Deref::All
                && !self.excludes.is_empty()
                && self.excludes.excludes(path))
    }

    /// Walk one directory below an operand from scratch.
    pub fn walk_dir(
        &self,
        path: &[u8],
        meta: Meta,
        root_dev: u64,
    ) -> Arc<Slot> {
        let slot = Arc::new(Slot::new());
        let context = Context {
            root_dev,
            previous: None,
        };
        rayon::scope(|scope| {
            self.list(scope, &context, path, meta, false, &slot, None);
        });
        slot
    }

    /// One directory's entries and their stats, read afresh.
    pub fn read_fresh(
        &self,
        path: &[u8],
        meta: Meta,
        is_root: bool,
    ) -> Listing {
        self.read(path, meta, is_root, None)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one task of the walk: the directory and where it sits"
    )]
    fn list<'s>(
        &'s self,
        scope: &rayon::Scope<'s>,
        context: &'s Context<'s>,
        path: &[u8],
        meta: Meta,
        is_root: bool,
        slot: &Arc<Slot>,
        above: Option<Arc<Above>>,
    ) {
        let root_dev = context.root_dev;
        let listing = self.read(path, meta, is_root, context.previous);
        let mut listing = listing;
        let here = Arc::new(Above {
            key: meta.key(),
            up: above,
        });
        for entry in &mut listing.entries {
            let Ok(child) = entry.meta else { continue };
            let child_path = join(path, &entry.name);
            if !self.enters(&child_path, &child, root_dev) {
                continue;
            }
            // With `-L` the shared listings below end any cycle.
            if self.deref != Deref::All && is_above(Some(&here), child.key()) {
                continue;
            }
            let child_slot = if self.deref == Deref::All {
                let mut seen = self
                    .seen
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(existing) = seen.get(&child.key()) {
                    entry.dir = Some(Arc::clone(existing));
                    continue;
                }
                let fresh = Arc::new(Slot::new());
                seen.insert(child.key(), Arc::clone(&fresh));
                fresh
            } else {
                Arc::new(Slot::new())
            };
            entry.dir = Some(Arc::clone(&child_slot));
            let above = Some(Arc::clone(&here));
            scope.spawn(move |scope| {
                self.list(
                    scope,
                    context,
                    &child_path,
                    child,
                    false,
                    &child_slot,
                    above,
                );
            });
        }
        let _ = slot.set(listing);
    }

    /// One directory's entries in `fts` order, each with its stat.
    fn read(
        &self,
        path: &[u8],
        meta: Meta,
        is_root: bool,
        previous: Option<&Previous>,
    ) -> Listing {
        let nofollow = match self.deref {
            Deref::Physical => true,
            Deref::Args => !is_root,
            Deref::All => false,
        };
        let mut flags = OFlags::RDONLY
            | OFlags::DIRECTORY
            | OFlags::CLOEXEC
            | OFlags::NONBLOCK
            | OFlags::NOCTTY;
        if nofollow {
            flags |= OFlags::NOFOLLOW;
        }
        let fd = match open_dir(path, flags) {
            Ok(fd) => fd,
            Err(errno) => {
                return Listing {
                    error: Some(ListError::Unreadable(errno)),
                    ..Listing::default()
                };
            }
        };
        let follow = self.deref == Deref::All;
        // Without links to follow, macOS hands over a whole directory's
        // stats at once, which is cheaper than any listing the index could
        // save a `readdir` of.
        #[cfg(target_os = "macos")]
        if let Some((items, failed)) = (!follow && crate::bulk::enabled())
            .then(|| crate::bulk::read(fd.as_fd()))
            .flatten()
        {
            let sort = inode_sort_useful(fd.as_fd());
            let error = failed.map(|errno| {
                if items.is_empty() {
                    ListError::Unreadable(errno)
                } else {
                    ListError::Partial(errno)
                }
            });
            let mut items = items;
            for batch in items.chunks_mut(READDIR_BATCH) {
                if sort && batch.len() > INODE_SORT_THRESHOLD {
                    batch.sort_by_key(|item| item.id);
                }
            }
            return Listing {
                entries: stat_missing(fd.as_fd(), items),
                error,
            };
        }
        if let Some(names) = previous.and_then(|previous| previous.names(meta))
        {
            return Listing {
                entries: stat_all(fd.as_fd(), names, follow),
                error: None,
            };
        }
        let sort = inode_sort_useful(fd.as_fd());
        // `Dir::new` takes the descriptor over; `read_from` would reopen
        // the directory to get one of its own, a system call per directory.
        let mut dir = match Dir::new(fd) {
            Ok(dir) => dir,
            Err(errno) => {
                return Listing {
                    error: Some(ListError::Unreadable(errno)),
                    ..Listing::default()
                };
            }
        };
        let mut names: Vec<(Box<[u8]>, u64)> = Vec::new();
        let mut error = None;
        let mut batch_start = 0;
        for item in &mut dir {
            match item {
                Ok(item) => {
                    let name = item.file_name().to_bytes();
                    if name == b"." || name == b".." {
                        continue;
                    }
                    names.push((name.into(), item.ino()));
                    if names.len() - batch_start == READDIR_BATCH {
                        sort_batch(&mut names[batch_start..], sort);
                        batch_start = names.len();
                    }
                }
                Err(errno) => {
                    error = Some(if names.is_empty() {
                        ListError::Unreadable(errno)
                    } else {
                        ListError::Partial(errno)
                    });
                    break;
                }
            }
        }
        sort_batch(&mut names[batch_start..], sort);
        let entries = match dir.fd() {
            Ok(fd) => stat_all(fd, names, follow),
            Err(errno) => {
                error = Some(ListError::Unreadable(errno));
                Vec::new()
            }
        };
        Listing { entries, error }
    }
}

fn sort_batch(batch: &mut [(Box<[u8]>, u64)], useful: bool) {
    if useful && batch.len() > INODE_SORT_THRESHOLD {
        batch.sort_by_key(|&(_, ino)| ino);
    }
}

/// Entries from the bulk listing, with `fstatat` for those it could not
/// describe exactly.
#[cfg(target_os = "macos")]
fn stat_missing(
    dir: BorrowedFd<'_>,
    items: Vec<crate::bulk::Item>,
) -> Vec<Entry> {
    let finish = |item: crate::bulk::Item| {
        let meta = match item.meta {
            Some(meta) => Ok(meta),
            None => stat_at(dir, &item.name, false),
        };
        Entry {
            name: item.name,
            d_ino: item.id,
            meta,
            dir: None,
        }
    };
    if items.len() >= PARALLEL_STATS {
        items.into_par_iter().map(finish).collect()
    } else {
        items.into_iter().map(finish).collect()
    }
}

fn stat_all(
    dir: BorrowedFd<'_>,
    names: Vec<(Box<[u8]>, u64)>,
    follow: bool,
) -> Vec<Entry> {
    let stat = |(name, d_ino): (Box<[u8]>, u64)| {
        let meta = stat_at(dir, &name, follow);
        Entry {
            name,
            d_ino,
            meta,
            dir: None,
        }
    };
    if names.len() >= PARALLEL_STATS {
        names.into_par_iter().map(stat).collect()
    } else {
        names.into_iter().map(stat).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operands_keep_one_trailing_slash() {
        assert_eq!(trim_operand(b"a///"), b"a/");
        assert_eq!(trim_operand(b"//"), b"//");
        assert_eq!(trim_operand(b"///"), b"/");
        assert_eq!(join(b"a/", b"b"), b"a/b");
        assert_eq!(join(b"a", b"b"), b"a/b");
    }
}

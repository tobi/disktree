//! Bringing a snapshot up to date without walking what did not change, for
//! `--max-age`.
//!
//! Two ways to know what changed, the better one when it is there:
//!
//! * The `FSEvents` journal (macOS). It names each directory whose entries
//!   changed or that holds a file written and closed since the snapshot.
//!   Those are read again; every other directory is taken from the
//!   snapshot as it is, without a system call.
//! * Directory timestamps (everywhere else, or when the journal cannot
//!   vouch for the whole interval). Every directory is stat'ed and read
//!   again when its ctime or mtime moved, which catches every entry created,
//!   removed or renamed. It does not see a file growing in place.
//!
//! Neither sees a file that is still open for writing: `FSEvents` reports the
//! write when the file is closed. So in both, files last modified within an
//! hour of the snapshot, the ones most likely to still be open, are stat'ed
//! again whatever else is known, and so are files with more than one link,
//! which can be written through a directory the journal names while the
//! link du counts sits in one it does not.

use std::sync::Arc;

use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use super::walk::{
    Entry, Listing, Meta, Root, Slot, Time, Walker, join, stat_path,
};

/// How recently before a snapshot a file must have been written for it to
/// be stat'ed again on every refresh.
const RECENT_SECS: i64 = 60 * 60;

/// What the journal says changed, as absolute paths.
#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
pub struct Changes {
    /// Directories whose entries, or whose files, changed.
    pub dirs: Vec<Vec<u8>>,
    /// Directories whose whole subtree must be looked at again: the
    /// journal coalesced what happened there.
    pub subtrees: Vec<Vec<u8>>,
}

/// What the journal says changed, relative to the operand.
#[derive(Debug, Default)]
pub struct Journal {
    dirs: FxHashSet<Vec<u8>>,
    subtrees: Vec<Vec<u8>>,
    /// Every directory a refresh has to look into: those named, the ones
    /// holding files to stat again, and all their ancestors.
    touched: FxHashSet<Vec<u8>>,
}

impl Journal {
    /// Turn the journal's absolute paths into paths below `base`, the
    /// operand's canonical path, ignoring anything outside it.
    #[cfg(target_os = "macos")]
    pub fn new(base: &[u8], changes: &Changes) -> Self {
        let relative = |path: &[u8]| -> Option<Vec<u8>> {
            if path == base {
                return Some(Vec::new());
            }
            let rest = path.strip_prefix(base)?;
            let rest = if base.ends_with(b"/") {
                rest
            } else {
                rest.strip_prefix(b"/")?
            };
            Some(rest.to_vec())
        };
        let mut journal = Self::default();
        for dir in changes.dirs.iter().filter_map(|dir| relative(dir)) {
            journal.touch(&dir);
            journal.dirs.insert(dir);
        }
        for dir in &changes.subtrees {
            // A subtree at or above the operand covers all of it.
            let covers = base.starts_with(dir)
                && (dir.ends_with(b"/")
                    || base.len() == dir.len()
                    || base.get(dir.len()) == Some(&b'/'));
            let rel = if covers {
                Some(Vec::new())
            } else {
                relative(dir)
            };
            if let Some(rel) = rel {
                journal.touch(&rel);
                journal.subtrees.push(rel);
            }
        }
        journal
    }

    /// Mark `rel` and every directory above it.
    fn touch(&mut self, rel: &[u8]) {
        let mut rel = rel;
        loop {
            if !self.touched.insert(rel.to_vec()) {
                return;
            }
            match rel.iter().rposition(|&b| b == b'/') {
                Some(at) => rel = &rel[..at],
                None if rel.is_empty() => return,
                None => rel = b"",
            }
        }
    }

    fn in_subtree(&self, rel: &[u8]) -> bool {
        self.subtrees.iter().any(|tree| {
            tree.is_empty()
                || rel == tree.as_slice()
                || (rel.starts_with(tree) && rel.get(tree.len()) == Some(&b'/'))
        })
    }

    fn dirty(&self, rel: &[u8]) -> bool {
        self.dirs.contains(rel) || self.in_subtree(rel)
    }

    fn visit(&self, rel: &[u8]) -> bool {
        self.touched.contains(rel) || self.in_subtree(rel)
    }
}

/// Whether the file an entry describes must be stat'ed again regardless.
fn recheck(meta: &Meta, recent: Time) -> bool {
    !meta.is_dir() && (meta.nlink > 1 || meta.mtime >= recent)
}

fn child_rel(rel: &[u8], name: &[u8]) -> Vec<u8> {
    if rel.is_empty() {
        name.to_vec()
    } else {
        let mut child = rel.to_vec();
        child.push(b'/');
        child.extend_from_slice(name);
        child
    }
}

/// Unchanged as far as its entries go. A directory whose timestamps were
/// within [`RACY_SECS`] of the snapshot's walk could have changed again in
/// the same tick after it was read, so it counts as changed: the index's
/// racy-clean rule.
fn same_dir(then: &Meta, now: &Meta, taken: Time) -> bool {
    let limit = Time {
        sec: taken.sec.saturating_sub(RACY_SECS),
        nsec: taken.nsec,
    };
    then.key() == now.key()
        && then.ctime == now.ctime
        && then.mtime == now.mtime
        && then.ctime < limit
        && then.mtime < limit
}

const RACY_SECS: i64 = 2;

struct Refresher<'a> {
    walker: &'a Walker<'a>,
    root_dev: u64,
    taken: Time,
    recent: Time,
    journal: Option<&'a Journal>,
}

/// Bring `root` up to date from `tree`, the snapshot's walk of it taken at
/// `taken`, using `journal` when there is one. Returns whether anything
/// differed from the snapshot.
pub fn refresh(
    walker: &Walker<'_>,
    root: &mut Root,
    tree: &Arc<Slot>,
    then: &Meta,
    taken: Time,
    journal: Option<&mut Journal>,
) -> bool {
    let Ok(now) = root.meta else { return true };
    let recent = Time {
        sec: taken.sec.saturating_sub(RECENT_SECS),
        nsec: taken.nsec,
    };
    // With a journal, the directories holding files to stat again have to
    // be visited too; find them in the snapshot.
    let journal = journal.map(|journal| {
        mark_rechecks(tree, b"", recent, now.dev, journal);
        &*journal
    });
    let refresher = Refresher {
        walker,
        root_dev: now.dev,
        taken,
        recent,
        journal,
    };
    let path = root.path.clone();
    let (slot, changed) = refresher.dir(&path, b"", tree, then, now, true);
    root.dir = Some(slot);
    changed || !then.unchanged(&now)
}

/// Mark the directories a journal catch-up has to visit although the
/// journal does not name them: those holding files to stat again, and
/// those on another file system, whose changes are in another journal.
fn mark_rechecks(
    slot: &Slot,
    rel: &[u8],
    recent: Time,
    dev: u64,
    journal: &mut Journal,
) {
    let Some(listing) = slot.get() else { return };
    let mut here = false;
    for entry in &listing.entries {
        match (&entry.meta, &entry.dir) {
            (Ok(meta), Some(dir)) if meta.is_dir() => {
                let child = child_rel(rel, &entry.name);
                if meta.dev == dev {
                    mark_rechecks(dir, &child, recent, dev, journal);
                } else {
                    journal.touch(&child);
                }
            }
            (Ok(meta), _) => here |= recheck(meta, recent),
            (Err(_), _) => {}
        }
    }
    if here {
        journal.touch(rel);
    }
}

impl Refresher<'_> {
    /// One directory: `then` is its stat in the snapshot, `now` its stat
    /// today. Returns its listing, and whether it or anything below it
    /// changed.
    fn dir(
        &self,
        path: &[u8],
        rel: &[u8],
        old: &Arc<Slot>,
        then: &Meta,
        now: Meta,
        is_root: bool,
    ) -> (Arc<Slot>, bool) {
        let Some(old_listing) = old.get() else {
            return (self.walker.walk_dir(path, now, self.root_dev), true);
        };
        let dirty = match self.journal {
            // Another file system's changes are in another journal.
            Some(journal) => journal.dirty(rel) || now.dev != self.root_dev,
            None => !same_dir(then, &now, self.taken),
        } || old_listing.error.is_some();
        if dirty {
            return (self.reread(path, rel, old_listing, now, is_root), true);
        }
        if self.journal.is_some_and(|journal| !journal.visit(rel)) {
            return (Arc::clone(old), false);
        }
        let results: Vec<(Entry, bool)> = old_listing
            .entries
            .par_iter()
            .map(|entry| self.entry(path, rel, entry))
            .collect();
        if results.iter().all(|(_, changed)| !changed) {
            return (Arc::clone(old), false);
        }
        let listing = Listing {
            entries: results.into_iter().map(|(entry, _)| entry).collect(),
            error: None,
        };
        (slot(listing), true)
    }

    /// One entry of a directory whose own entries have not changed.
    fn entry(&self, path: &[u8], rel: &[u8], entry: &Entry) -> (Entry, bool) {
        let Ok(then) = entry.meta else {
            return (entry.clone(), false);
        };
        let entry_path = join(path, &entry.name);
        if !then.is_dir() {
            if !recheck(&then, self.recent) {
                return (entry.clone(), false);
            }
            let now = stat_path(&entry_path, false);
            let changed = !matches!(now, Ok(now) if now.unchanged(&then));
            return (
                Entry {
                    meta: now,
                    ..entry.clone()
                },
                changed,
            );
        }
        let Some(old) = &entry.dir else {
            return (entry.clone(), false);
        };
        let rel = child_rel(rel, &entry.name);
        if self.journal.is_some_and(|journal| !journal.visit(&rel)) {
            return (entry.clone(), false);
        }
        let now = stat_path(&entry_path, false);
        let (dir, changed) = match now {
            Ok(now) if now.is_dir() && now.key() == then.key() => {
                let (dir, changed) =
                    self.dir(&entry_path, &rel, old, &then, now, false);
                (Some(dir), changed || !now.unchanged(&then))
            }
            Ok(now) if self.walker.enters(&entry_path, &now, self.root_dev) => {
                // Another directory under the same name, between two looks
                // at its parent: walk it as new.
                (
                    Some(self.walker.walk_dir(&entry_path, now, self.root_dev)),
                    true,
                )
            }
            _ => (None, true),
        };
        (
            Entry {
                meta: now,
                dir,
                ..entry.clone()
            },
            changed,
        )
    }

    /// Read a directory again, keeping what can be kept below it.
    fn reread(
        &self,
        path: &[u8],
        rel: &[u8],
        old: &Listing,
        now: Meta,
        is_root: bool,
    ) -> Arc<Slot> {
        let mut listing = self.walker.read_fresh(path, now, is_root);
        let by_name: FxHashMap<&[u8], &Entry> = old
            .entries
            .iter()
            .map(|entry| (&*entry.name, entry))
            .collect();
        listing.entries.par_iter_mut().for_each(|entry| {
            let Ok(meta) = entry.meta else { return };
            let entry_path = join(path, &entry.name);
            if !self.walker.enters(&entry_path, &meta, self.root_dev) {
                return;
            }
            let before = by_name.get(&*entry.name).copied().filter(|old| {
                old.meta.is_ok_and(|then| then.key() == meta.key())
            });
            entry.dir = Some(match before {
                Some(Entry {
                    meta: Ok(then),
                    dir: Some(old),
                    ..
                }) => {
                    let rel = child_rel(rel, &entry.name);
                    self.dir(&entry_path, &rel, old, then, meta, false).0
                }
                _ => self.walker.walk_dir(&entry_path, meta, self.root_dev),
            });
        });
        slot(listing)
    }
}

fn slot(listing: Listing) -> Arc<Slot> {
    let slot = Arc::new(Slot::new());
    let _ = slot.set(listing);
    slot
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(sec: i64) -> Meta {
        let time = Time { sec, nsec: 0 };
        Meta {
            dev: 1,
            ino: 2,
            mode: 0o040_755,
            nlink: 2,
            blocks: 0,
            size: 64,
            mtime: time,
            atime: time,
            ctime: time,
        }
    }

    #[test]
    fn a_journal_catch_up_visits_other_file_systems() {
        let mut mounted = dir(50);
        mounted.dev = 9;
        let tree = slot(Listing {
            entries: vec![
                Entry {
                    name: b"same".as_slice().into(),
                    d_ino: 2,
                    meta: Ok(dir(50)),
                    dir: Some(slot(Listing::default())),
                },
                Entry {
                    name: b"mount".as_slice().into(),
                    d_ino: 3,
                    meta: Ok(mounted),
                    dir: Some(slot(Listing::default())),
                },
            ],
            error: None,
        });
        let mut journal = Journal::default();
        mark_rechecks(&tree, b"", Time { sec: 0, nsec: 0 }, 1, &mut journal);
        assert!(
            journal.visit(b"mount"),
            "its changes are not in this journal"
        );
        assert!(!journal.visit(b"same"));
    }

    #[test]
    fn a_directory_changed_near_the_walk_counts_as_changed() {
        let taken = Time { sec: 100, nsec: 0 };
        assert!(same_dir(&dir(50), &dir(50), taken));
        assert!(!same_dir(&dir(50), &dir(51), taken), "timestamps moved");
        assert!(
            !same_dir(&dir(99), &dir(99), taken),
            "within the racy window"
        );
    }
}

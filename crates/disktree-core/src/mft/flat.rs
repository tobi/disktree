//! The file table reader's side of the tree ([`crate::tree`]): building
//! it from a whole read of the table, and bringing one kept on disk up to
//! date.
//!
//! A tree built here is what a scan hands over and what the next scan
//! loads. It replaces the entries of the files the change journal names,
//! totals and orders again only the directories that hold a change and
//! those above them, and decides kinds again only where a change can
//! reach: a journal of a few thousand changes costs thousands of steps,
//! not the millions a tree built from the table again would.
//!
//! A kept tree holds only what it shows, so a directory that comes into
//! view with entries it never held (a cloud folder made local, a folder
//! moved in from a hidden one) cannot be brought up to date from it: the
//! table is read whole instead. One made since the tree was is fine, as
//! every entry it has was made or moved in since too, and the journal
//! names each.

use std::sync::atomic::{AtomicBool, Ordering};

use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

use super::{Entry, FIRST_USER_RECORD, Info, MOST_LEVELS, ROOT, Stop, Table};
use crate::scan::ScanOptions;
use crate::tree::{
    Builder, DIRECTORY, Dir, FILE, IDENTIFIED, Item, LINK, NONE, Seg, Totals,
    Tree, name_in, order, seconds,
};

/// A file reference as NTFS writes one: the record, and in the top 16
/// bits the sequence number the record had. What a tree built here keeps
/// as each entry's and directory's `id`.
pub(super) const fn reference(record: u32, sequence: u16) -> u64 {
    record as u64 | (sequence as u64) << 48
}

pub(super) const fn record(id: u64) -> u32 {
    id as u32
}

pub(super) const fn sequence(id: u64) -> u16 {
    (id >> 48) as u16
}

/// The `id` of a directory dropped from the tree.
const GONE: u64 = reference(NONE, 0);

const fn is_live(dir: &Dir) -> bool {
    record(dir.id) != NONE
}

/// Levels whose directories are built in parallel. A top level already
/// splits a disk into enough work for every thread; below it a parallel
/// frame per level would only cut the depth that fits in a worker's stack.
const PARALLEL_LEVELS: usize = 4;

/// A file record as NTFS holds it now.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Fresh {
    pub info: Info,
    /// Every name: `(parent, parent's sequence number, name)`.
    pub names: Vec<(u32, u16, String)>,
}

/// Whether a scan with `options` leaves the name out, as the walk does.
fn hidden(options: &ScanOptions, name: &str, info: &Info) -> bool {
    !options.include_hidden && (name.starts_with('.') || info.hidden)
}

/// Put `name` in `text` as `item`'s.
fn name_into(
    text: &mut String,
    item: &mut Item,
    name: &str,
) -> Result<(), Stop> {
    item.at = u32::try_from(text.len()).map_err(|_| Stop)?;
    item.len = u16::try_from(name.len()).map_err(|_| Stop)?;
    text.push_str(name);
    Ok(())
}

impl Tree {
    /// Bring the tree up to date: `numbers` are the records that changed,
    /// sorted, and `fresh` what those still in use hold now. `created`
    /// tells a record made since the tree was. Totals and order come up to
    /// date too; kinds are left to `classify::classify_where`, given what
    /// this returns: the directories whose entries changed and every one
    /// above them. `None` when the tree cannot be brought up to date from
    /// what it holds: a directory came into view whose entries it never
    /// held, a directory has two names, or the changes do not make a tree,
    /// or make one deeper than a whole read goes. That holds the kept tree
    /// too, which a scan resumes only through here.
    pub(super) fn patch(
        &mut self,
        numbers: &[u32],
        fresh: &FxHashMap<u32, Fresh>,
        created: impl Fn(u32) -> bool,
        options: &ScanOptions,
    ) -> Option<Vec<bool>> {
        // The root and the file system's own records are never entries.
        let numbers: Vec<u32> = numbers
            .iter()
            .copied()
            .filter(|&number| number >= FIRST_USER_RECORD)
            .collect();
        let changed = Bits::of(&numbers);
        let parents: FxHashSet<u32> = numbers
            .iter()
            .filter_map(|number| fresh.get(number))
            .flat_map(|fresh| fresh.names.iter().map(|&(parent, ..)| parent))
            .collect();

        // The directory each changed record, and each parent a name
        // names, was.
        let found: Vec<(u32, u32)> = self
            .dirs
            .par_iter()
            .enumerate()
            .filter(|(_, dir)| {
                is_live(dir)
                    && (changed.has(record(dir.id))
                        || parents.contains(&record(dir.id)))
            })
            .map(|(index, dir)| (record(dir.id), index as u32))
            .collect();
        let mut dir_of = FxHashMap::default();
        for (number, index) in found {
            if dir_of.insert(number, index).is_some() {
                return None;
            }
        }

        // The entries changed records had, by where they are: their
        // segment and place in it.
        let old: Vec<(u32, (u32, u32))> = self
            .dirs
            .par_iter()
            .enumerate()
            .filter(|(_, dir)| is_live(dir))
            .flat_map_iter(|(index, dir)| {
                let (seg, first) = (dir.seg, dir.first);
                self.run(dir)
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| changed.has(record(item.id)))
                    .map(move |(at, _)| {
                        (index as u32, (seg, first + at as u32))
                    })
            })
            .collect();
        let mut dirty: FxHashSet<u32> =
            old.iter().map(|&(dir, _)| dir).collect();
        let removed: FxHashSet<(u32, u32)> =
            old.iter().map(|&(_, at)| at).collect();

        // The runs this changes go into a segment of their own, after
        // every other: nothing already there moves.
        let seg = u32::try_from(self.segs.len()).ok()?;
        self.segs.push(Seg::default());

        // A changed record that is a shown directory now keeps its place
        // in `dirs` if it had one at the same sequence number, and gets a
        // new one if it was made since the tree was.
        let mut placed: FxHashMap<u32, u32> = FxHashMap::default();
        let mut unknown = FxHashSet::default();
        for &number in &numbers {
            let Some(Fresh { info, .. }) = fresh.get(&number) else {
                continue;
            };
            if !info.in_use || !info.directory || info.is_link() || info.evicted
            {
                continue;
            }
            match dir_of.get(&number) {
                Some(&index)
                    if sequence(self.dirs[index as usize].id)
                        == info.sequence =>
                {
                    placed.insert(number, index);
                }
                _ if created(number) => {
                    let index = u32::try_from(self.dirs.len()).ok()?;
                    self.dirs.push(Dir {
                        id: reference(number, info.sequence),
                        seg,
                        ..Dir::EMPTY
                    });
                    placed.insert(number, index);
                    dirty.insert(index);
                }
                _ => {
                    unknown.insert(number);
                }
            }
        }

        // The entries changed records have now, as the build makes them.
        let mut added: FxHashMap<u32, Vec<Item>> = FxHashMap::default();
        let mut attached = FxHashSet::default();
        for &number in &numbers {
            let Some(Fresh { info, names }) = fresh.get(&number) else {
                continue;
            };
            let directory = info.directory && !info.is_link();
            if !info.in_use || directory && info.evicted {
                continue;
            }
            let mut charged = false;
            for (parent, parent_sequence, name) in names {
                if *parent == number || hidden(options, name, info) {
                    continue;
                }
                let holder = if changed.has(*parent) {
                    placed.get(parent)
                } else {
                    dir_of.get(parent)
                };
                let Some(&holder) = holder else {
                    continue;
                };
                if sequence(self.dirs[holder as usize].id) != *parent_sequence {
                    continue;
                }
                let mut item = if directory {
                    let Some(&child) = placed.get(&number) else {
                        if unknown.contains(&number) {
                            return None;
                        }
                        continue;
                    };
                    if !attached.insert(child) {
                        return None;
                    }
                    self.dirs[child as usize].parent = holder;
                    Item {
                        id: reference(number, info.sequence),
                        kind: DIRECTORY,
                        value: u64::from(child),
                        ..Item::default()
                    }
                } else {
                    let mut size = if options.apparent_size {
                        info.apparent
                    } else {
                        info.allocated
                    };
                    let shared = info.names > 1;
                    // One of a hardlinked file's names weighs, as the
                    // build charges the first it meets.
                    if options.dedup_hardlinks && shared && size > 0 {
                        if charged {
                            size = 0;
                        }
                        charged = true;
                    }
                    Item {
                        id: reference(number, info.sequence),
                        kind: if info.is_link() { LINK } else { FILE },
                        flags: if shared { IDENTIFIED } else { 0 },
                        value: size,
                        modified: seconds(info.modified),
                        ..Item::default()
                    }
                };
                name_into(&mut self.segs[seg as usize].text, &mut item, name)
                    .ok()?;
                added.entry(holder).or_default().push(item);
                dirty.insert(holder);
            }
        }

        // A changed record's directory with no place now went, or left the
        // view, with everything that stayed beneath it.
        let gone: Vec<u32> = dir_of
            .iter()
            .filter(|&(&number, index)| {
                changed.has(number) && !attached.contains(index)
            })
            .map(|(_, &index)| index)
            .chain(
                placed
                    .values()
                    .copied()
                    .filter(|index| !attached.contains(index)),
            )
            .collect();
        for index in gone {
            // A file charged to a name that went, whose other names stay,
            // is not read again: only a whole read charges one of those.
            if self.drop_beneath(index, &removed) && options.dedup_hardlinks {
                return None;
            }
        }

        // Each changed directory's entries again, as a run of their own in
        // the new segment: what it kept, then what it gained.
        let mut dirty: Vec<u32> = dirty
            .into_iter()
            .filter(|&index| self.dirs.get(index as usize).is_some_and(is_live))
            .collect();
        dirty.sort_unstable();
        for &index in &dirty {
            let dir = self.dirs[index as usize];
            let (older, newer) = self.segs.split_at_mut(seg as usize);
            let into = &mut newer[0];
            let first = u32::try_from(into.items.len()).ok()?;
            if let Some(from) = older.get(dir.seg as usize) {
                for at in dir.first..dir.first.saturating_add(dir.len) {
                    if !removed.contains(&(dir.seg, at))
                        && let Some(&item) = from.items.get(at as usize)
                    {
                        let mut moved = item;
                        name_into(
                            &mut into.text,
                            &mut moved,
                            name_in(&from.text, &item),
                        )
                        .ok()?;
                        into.items.push(moved);
                    }
                }
            }
            into.items.extend(added.remove(&index).unwrap_or_default());
            let len = u32::try_from(into.items.len()).ok()? - first;
            let dir = &mut self.dirs[index as usize];
            dir.seg = seg;
            dir.first = first;
            dir.len = len;
        }

        // Every directory given a place must hang from the root.
        for &index in &attached {
            let mut at = index;
            let mut steps = 0;
            while at != 0 {
                let dir = self.dirs.get(at as usize)?;
                if !is_live(dir) || steps > MOST_LEVELS {
                    return None;
                }
                at = dir.parent;
                steps += 1;
            }
        }
        // Nor deeper than a whole read goes: a folder moved takes all it
        // holds down with it, and a kept tree comes only through here.
        if !self.within_levels() {
            return None;
        }

        // Totals and order again for each changed directory and those
        // above it, deepest first, so each totals children already done.
        let mut touched = vec![false; self.dirs.len()];
        let mut chain = Vec::new();
        for &index in &dirty {
            let mut at = index;
            while let Some(flag) = touched.get_mut(at as usize)
                && !*flag
            {
                *flag = true;
                chain.push(at);
                at = self.dirs[at as usize].parent;
            }
        }
        let depth = |mut at: u32| {
            let mut depth = 0;
            while let Some(dir) = self.dirs.get(at as usize)
                && depth <= MOST_LEVELS
            {
                at = dir.parent;
                depth += 1;
            }
            depth
        };
        let mut chain: Vec<(usize, u32)> = chain
            .into_iter()
            .map(|index| (depth(index), index))
            .collect();
        chain.sort_unstable_by(|left, right| right.cmp(left));
        for (_, index) in chain {
            self.settle(index, options.metric);
        }
        Some(touched)
    }

    /// Drop directory `index` and everything beneath it that stayed
    /// there: an entry in `removed` has another place now, or none.
    /// Whether that dropped a file with more names by the one it is
    /// charged to, which leaves the others weighing nothing.
    fn drop_beneath(
        &mut self,
        index: u32,
        removed: &FxHashSet<(u32, u32)>,
    ) -> bool {
        let mut charged = false;
        let mut stack = vec![index];
        while let Some(index) = stack.pop() {
            let Some(dir) = self.dirs.get_mut(index as usize) else {
                continue;
            };
            if !is_live(dir) {
                continue;
            }
            let (seg, first, len) = (dir.seg, dir.first, dir.len);
            *dir = Dir {
                id: GONE,
                parent: NONE,
                len: 0,
                ..*dir
            };
            let Some(from) = self.segs.get(seg as usize) else {
                continue;
            };
            for at in first..first.saturating_add(len) {
                let Some(item) = from.items.get(at as usize) else {
                    continue;
                };
                if removed.contains(&(seg, at)) {
                    continue;
                }
                if item.is_dir() {
                    if let Ok(child) = u32::try_from(item.value) {
                        stack.push(child);
                    }
                } else if item.identified() && item.value > 0 {
                    charged = true;
                }
            }
        }
        charged
    }

    /// Whether a patch left entries, names or directories behind that the
    /// tree no longer shows.
    pub(super) fn has_garbage(&self) -> bool {
        let shown: u64 = self
            .dirs
            .iter()
            .filter(|dir| is_live(dir))
            .map(|dir| u64::from(dir.len))
            .sum();
        let held: u64 =
            self.segs.iter().map(|seg| seg.items.len() as u64).sum();
        shown != held || self.dirs.iter().any(|dir| !is_live(dir))
    }

    /// The tree without what patches left behind, in one segment, its
    /// directories in the order a walk from the root meets them; `None`
    /// when one segment cannot number all it holds.
    pub(super) fn compact(&self) -> Option<Self> {
        let items = self.segs.iter().map(|seg| seg.items.len()).sum();
        let text = self.segs.iter().map(|seg| seg.text.len()).sum();
        let mut into = Seg {
            items: Vec::with_capacity(items),
            text: String::with_capacity(text),
        };
        let mut dirs = Vec::with_capacity(self.dirs.len());
        let Some(&root) = self.dirs.first() else {
            return Some(Self::default());
        };
        dirs.push(root);
        // Where each directory of `dirs` was.
        let mut from = vec![0_u64];
        let mut at = 0;
        while let Some(dir) = from.get(at).and_then(|&old| self.dir(old)) {
            let first = u32::try_from(into.items.len()).ok()?;
            for item in self.run(dir) {
                let mut item = *item;
                let name = self.text(dir.seg, &item);
                item.at = u32::try_from(into.text.len()).ok()?;
                into.text.push_str(name);
                if item.is_dir() {
                    let Some(child) = self.dir(item.value) else {
                        continue;
                    };
                    from.push(item.value);
                    item.value = dirs.len() as u64;
                    dirs.push(Dir {
                        parent: u32::try_from(at).ok()?,
                        ..*child
                    });
                }
                into.items.push(item);
            }
            let dir = &mut dirs[at];
            dir.seg = 0;
            dir.first = first;
            dir.len = u32::try_from(into.items.len()).ok()? - first;
            at += 1;
        }
        Some(Self {
            name: self.name.clone(),
            dirs,
            segs: vec![into],
            volumes: self.volumes.clone(),
        })
    }

    /// `(record, sequence, size)` of the `count` largest files the tree
    /// shows, which must hold nothing it does not show: see
    /// [`Tree::compact`].
    pub(super) fn largest(&self, count: usize) -> Vec<(u32, u16, u64)> {
        let mut files: Vec<(u64, u32, u16)> = self
            .segs
            .par_iter()
            .flat_map_iter(|seg| &seg.items)
            .filter(|item| !item.is_dir())
            .map(|item| (item.value, record(item.id), sequence(item.id)))
            .collect();
        if files.len() > count {
            files.select_nth_unstable_by(count, |left, right| right.cmp(left));
            files.truncate(count);
        }
        let mut largest: Vec<(u32, u16, u64)> = files
            .into_iter()
            .map(|(size, record, sequence)| (record, sequence, size))
            .collect();
        // A file with more names that each weigh is one file.
        largest.sort_unstable();
        largest.dedup_by_key(|&mut (record, ..)| record);
        largest
    }

    /// Whether this is a tree: every run and name in bounds, every
    /// directory named once, by an entry of the directory it says holds
    /// it, and the root by none. A kept tree is a file another program can
    /// write, and must not become a tree that never ends.
    pub(super) fn is_valid(&self) -> bool {
        let named: Vec<AtomicBool> =
            std::iter::repeat_with(|| AtomicBool::new(false))
                .take(self.dirs.len())
                .collect();
        self.dirs
            .first()
            .is_some_and(|root| is_live(root) && root.parent == NONE)
            && self.dirs.par_iter().enumerate().all(|(index, dir)| {
                if !is_live(dir) {
                    return dir.len == 0;
                }
                let Some(seg) = self.segs.get(dir.seg as usize) else {
                    return false;
                };
                let Some(run) = seg
                    .items
                    .get(dir.first as usize..)
                    .and_then(|rest| rest.get(..dir.len as usize))
                else {
                    return false;
                };
                run.iter().all(|item| {
                    let at = item.at as usize;
                    seg.text.get(at..at + usize::from(item.len)).is_some()
                        && match item.kind {
                            FILE | LINK => true,
                            DIRECTORY => usize::try_from(item.value)
                                .ok()
                                .filter(|&child| child != 0)
                                .is_some_and(|child| {
                                    self.dirs.get(child).is_some_and(|sub| {
                                        is_live(sub)
                                            && sub.parent as usize == index
                                    }) && !named[child]
                                        .swap(true, Ordering::Relaxed)
                                }),
                            _ => false,
                        }
                })
            })
    }

    /// Whether every directory in the tree hangs from the root fewer than
    /// [`MOST_LEVELS`] below it, as a whole read of the table builds them,
    /// so what walks a tree on a thread's stack is not too deep for it.
    /// Up each directory's parents, keeping every level found, so each is
    /// climbed once: a pass over the directories, not over every entry,
    /// which took a warm launch 20 ms more.
    fn within_levels(&self) -> bool {
        // 0 for a level not known yet; only the root's is 0.
        let mut levels = vec![0_u16; self.dirs.len()];
        let mut chain = Vec::new();
        for (index, dir) in self.dirs.iter().enumerate().skip(1) {
            if !is_live(dir) || levels[index] != 0 {
                continue;
            }
            let mut at = index;
            let mut level = loop {
                // Deeper than allowed, or parents that loop.
                if chain.len() >= MOST_LEVELS {
                    return false;
                }
                chain.push(at);
                let parent = self.dirs[at].parent as usize;
                if parent == 0 {
                    break 0;
                }
                match levels.get(parent) {
                    Some(&0) => at = parent,
                    Some(&known) => break known,
                    // Hanging from nothing.
                    None => return false,
                }
            };
            while let Some(at) = chain.pop() {
                level += 1;
                if usize::from(level) >= MOST_LEVELS {
                    return false;
                }
                levels[at] = level;
            }
        }
        true
    }
}

/// Record numbers, a bit each: tested for every entry of the tree.
struct Bits(Vec<u64>);

impl Bits {
    fn of(numbers: &[u32]) -> Self {
        let top = numbers.iter().max().map_or(0, |&top| top as usize + 1);
        let mut bits = vec![0_u64; top.div_ceil(64)];
        for &number in numbers {
            bits[number as usize / 64] |= 1 << (number % 64);
        }
        Self(bits)
    }

    fn has(&self, number: u32) -> bool {
        self.0
            .get(number as usize / 64)
            .is_some_and(|word| word & (1 << (number % 64)) != 0)
    }
}

/// What one of a directory's entries becomes.
pub(super) enum Built {
    File(Item),
    /// A directory: its record and sequence number.
    Directory(u32, u16),
}

impl Table<'_> {
    /// The tree beneath the root, finished but for kinds, built on every
    /// thread of the pool it runs on: each directory's entries go to the
    /// tree once they are ordered, and nothing is copied again after.
    pub(super) fn build(&self) -> Result<Tree, Stop> {
        let sequence = self
            .infos
            .get(ROOT as usize)
            .map_or(0, |info| info.sequence);
        let build = Builder::new(rayon::current_num_threads());
        let root = build.reserve(1).ok_or(Stop)?;
        self.fill(&build, ROOT, sequence, root, NONE, 0)?;
        let mut tree = build.finish(Box::default());
        // Identities are record numbers of this one volume.
        tree.volumes = vec![0];
        Ok(tree)
    }

    pub(super) fn descend(&self, depth: usize) -> bool {
        self.options.max_depth.is_none_or(|max| depth < max)
    }

    /// What `entry`, in a directory at `sequence`, becomes; `None` for one
    /// the walk would not list. Mirrors what the walk keeps: see
    /// `WalkContext::classify`.
    pub(super) fn entry(
        &self,
        entry: &Entry,
        sequence: u16,
        descend: bool,
    ) -> Result<Option<Built>, Stop> {
        // A root directory holds hundreds of thousands of files; a check
        // per entry lets a cancel stop within one.
        if self.progress.is_cancelled() {
            return Err(Stop);
        }
        // An entry naming a parent whose record has since been reused, or
        // itself: the root is its own parent.
        if entry.parent_sequence != sequence
            || entry.child == entry.parent
            || entry.child < FIRST_USER_RECORD
        {
            return Ok(None);
        }
        let Some(info) = self.infos.get(entry.child as usize) else {
            return Ok(None);
        };
        if !info.in_use || hidden(self.options, self.name(entry), info) {
            return Ok(None);
        }
        let link = info.is_link();
        if info.directory && !link {
            if info.evicted || !descend {
                return Ok(None);
            }
            return Ok(Some(Built::Directory(entry.child, info.sequence)));
        }
        let mut size = if self.options.apparent_size {
            info.apparent
        } else {
            info.allocated
        };
        let shared = info.names > 1;
        // Every name of a file has the file's size, so one that weighs
        // nothing need not be remembered to be charged once.
        if size > 0
            && shared
            && let Some(seen) = &self.seen
            && !seen.insert((0, u64::from(entry.child)))
        {
            size = 0;
        }
        Ok(Some(Built::File(Item {
            id: reference(entry.child, info.sequence),
            kind: if link { LINK } else { FILE },
            flags: if shared { IDENTIFIED } else { 0 },
            value: size,
            modified: seconds(info.modified),
            ..Item::default()
        })))
    }

    /// Directory `number`, at `sequence`, and everything beneath it into
    /// `build`, as directory `index` beneath `parent`; returns its totals.
    /// Its entries are decided, its subdirectories built (at once, near
    /// the top), then its entries ordered and placed.
    fn fill(
        &self,
        build: &Builder,
        number: u32,
        sequence: u16,
        index: u32,
        parent: u32,
        depth: usize,
    ) -> Result<Totals, Stop> {
        if depth >= MOST_LEVELS || self.progress.is_cancelled() {
            return Err(Stop);
        }
        let descend = self.descend(depth);
        let metric = self.options.metric;
        let entries = self.entries(number);
        let mut run: Vec<(Item, &str)> = Vec::with_capacity(entries.len());
        // Each subdirectory: its record, sequence and place in `run`.
        let mut subdirs: Vec<(u32, u16, usize)> = Vec::new();
        for entry in entries {
            match self.entry(entry, sequence, descend)? {
                None => {}
                Some(Built::File(item)) => run.push((item, self.name(entry))),
                Some(Built::Directory(child, child_sequence)) => {
                    subdirs.push((child, child_sequence, run.len()));
                    let item = Item {
                        id: reference(child, child_sequence),
                        kind: DIRECTORY,
                        ..Item::default()
                    };
                    run.push((item, self.name(entry)));
                }
            }
        }
        let first = if subdirs.is_empty() {
            0
        } else {
            u32::try_from(subdirs.len())
                .ok()
                .and_then(|count| build.reserve(count))
                .ok_or(Stop)?
        };
        let fill =
            |(at, &(child, child_sequence, _)): (usize, &(u32, u16, usize))| {
                self.fill(
                    build,
                    child,
                    child_sequence,
                    first + at as u32,
                    index,
                    depth + 1,
                )
            };
        let sums: Vec<Totals> = if depth < PARALLEL_LEVELS {
            subdirs
                .par_iter()
                .enumerate()
                .map(fill)
                .collect::<Result<_, _>>()?
        } else {
            subdirs
                .iter()
                .enumerate()
                .map(fill)
                .collect::<Result<_, _>>()?
        };
        let mut totals = Totals::DIRECTORY;
        for (item, _) in &mut run {
            if !item.is_dir() {
                totals.add(item.totals());
            }
        }
        for (at, (sum, &(.., place))) in sums.iter().zip(&subdirs).enumerate() {
            run[place].0.value = u64::from(first + at as u32);
            totals.add(*sum);
        }
        let key = |item: &Item| {
            if item.is_dir() {
                let sum = &sums[(item.value as u32 - first) as usize];
                match metric {
                    crate::tree::Metric::Bytes => sum.bytes,
                    crate::tree::Metric::Files => sum.files,
                }
            } else {
                item.key(metric)
            }
        };
        // Unstable: names in one directory are distinct, and the only
        // ties are names that decoded to the same lossy text.
        run.sort_unstable_by(|(left, left_name), (right, right_name)| {
            order(
                (key(left), left_name.as_bytes()),
                (key(right), right_name.as_bytes()),
            )
        });
        let text = run.iter().map(|(_, name)| name.len()).sum();
        let mut dir = Dir {
            id: reference(number, sequence),
            parent,
            ..Dir::EMPTY
        };
        dir.set_totals(totals);
        build.place(index, dir, run.into_iter(), text).ok_or(Stop)?;
        Ok(totals)
    }
}

#[cfg(test)]
mod tests {
    use super::super::EVICTED;
    use std::collections::{BTreeMap, BTreeSet};
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_HIDDEN;

    use super::*;
    use crate::classify::{Category, Reclaim, classify, classify_where};
    use crate::scan::ScanProgress;
    use crate::tree::{Metric, Node, Seen};

    /// A volume: what each record in use holds, the root's (5, at
    /// sequence 5) aside.
    type Volume = BTreeMap<u32, Fresh>;

    fn dir(sequence: u16, (parent, at): (u32, u16), name: &str) -> Fresh {
        Fresh {
            info: Info {
                in_use: true,
                directory: true,
                sequence,
                names: 1,
                ..Info::default()
            },
            names: vec![(parent, at, name.to_owned())],
        }
    }

    fn file(sequence: u16, names: &[(u32, u16, &str)], size: u64) -> Fresh {
        Fresh {
            info: Info {
                in_use: true,
                sequence,
                names: names.len() as u8,
                apparent: size,
                allocated: size.next_multiple_of(4096),
                modified: size.cast_signed() % 1000,
                ..Info::default()
            },
            names: names
                .iter()
                .map(|&(parent, at, name)| (parent, at, name.to_owned()))
                .collect(),
        }
    }

    fn table<'a>(
        volume: &Volume,
        options: &'a ScanOptions,
        progress: &'a ScanProgress,
    ) -> Table<'a> {
        let top = volume.keys().max().map_or(0, |&top| top as usize) + 1;
        let mut infos = vec![Info::default(); top.max(ROOT as usize + 1)];
        infos[ROOT as usize] = Info {
            in_use: true,
            directory: true,
            sequence: 5,
            ..Info::default()
        };
        let mut text = String::new();
        let mut names = Vec::new();
        for (&child, fresh) in volume {
            infos[child as usize] = fresh.info;
            for (parent, parent_sequence, name) in &fresh.names {
                names.push(Entry {
                    parent: *parent,
                    parent_sequence: *parent_sequence,
                    child,
                    chunk: 0,
                    at: text.len() as u32,
                    len: name.len() as u16,
                });
                text.push_str(name);
            }
        }
        names.sort_by_key(|entry| entry.parent);
        let starts = super::super::starts(&names, infos.len());
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

    /// The tree a whole read of `volume` makes, classified.
    fn built(volume: &Volume, options: &ScanOptions) -> Tree {
        let progress = ScanProgress::default();
        let table = table(volume, options, &progress);
        let Ok(mut tree) = table.build() else {
            panic!("the table makes a tree");
        };
        classify(&mut tree);
        assert!(tree.is_valid());
        tree
    }

    /// `tree`, the tree of `before`, brought up to `after` the way a
    /// resumed scan does: the journal names what differs, and records made
    /// since, new or reused, are what it saw created.
    fn patch(
        tree: &mut Tree,
        before: &Volume,
        after: &Volume,
        options: &ScanOptions,
    ) -> Option<()> {
        let numbers: Vec<u32> = before
            .keys()
            .chain(after.keys())
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|number| before.get(number) != after.get(number))
            .collect();
        let fresh: FxHashMap<u32, Fresh> = numbers
            .iter()
            .filter_map(|&number| Some((number, after.get(&number)?.clone())))
            .collect();
        let created = |number: u32| {
            after.get(&number).is_some_and(|now| {
                before
                    .get(&number)
                    .is_none_or(|was| was.info.sequence != now.info.sequence)
            })
        };
        let touched = tree.patch(&numbers, &fresh, created, options)?;
        classify_where(tree, Some(&touched));
        Some(())
    }

    /// Every node as a line, in order: its path and all it says. Checks
    /// on the way that the kinds are what `classify` makes of the tree.
    fn lines(tree: &Tree) -> Vec<String> {
        let mut again = tree.clone();
        classify(&mut again);
        let mut out = Vec::new();
        describe(tree.root(), "", &mut out);
        let mut expected = Vec::new();
        describe(again.root(), "", &mut expected);
        assert_eq!(out, expected, "kinds as `classify` decides them");
        out
    }

    fn describe(node: Node<'_>, path: &str, out: &mut Vec<String>) {
        out.push(format!(
            "{path} {:?} {} {} {} {} {} {:?} {} {:?} {:?}",
            node.kind(),
            node.bytes(),
            node.own_bytes(),
            node.files(),
            node.own_files(),
            node.dirs(),
            node.inode(),
            node.modified(),
            node.category(),
            node.reclaim()
        ));
        for child in node.children() {
            describe(child, &format!("{path}/{}", child.name()), out);
        }
    }

    fn exact() -> ScanOptions {
        // Which name of a hardlinked file weighs is whichever the build
        // meets first: compared exactly, each weighs.
        ScanOptions {
            dedup_hardlinks: false,
            ..ScanOptions::default()
        }
    }

    #[test]
    fn patching_the_changed_files_gives_what_a_whole_read_would() {
        let root = (ROOT, 5);
        let mut before = Volume::new();
        for (number, fresh) in [
            (20, dir(1, root, "src")),
            (21, dir(1, (20, 1), "rust-thing")),
            (22, dir(1, (21, 1), "target")),
            (23, file(1, &[(22, 1, "out.bin")], 5000)),
            (24, dir(1, root, "repo")),
            (25, dir(1, (24, 1), "objects")),
            (26, dir(1, (24, 1), "refs")),
            (27, dir(1, root, "mystery")),
            (60, dir(1, (27, 1), "src")),
            (61, file(1, &[(60, 1, "small")], 100)),
            (30, dir(1, (27, 1), ".cache")),
            (31, file(1, &[(30, 1, "blob")], 5000)),
            (40, file(1, &[(5, 5, "notes.txt")], 300)),
            (
                41,
                file(1, &[(20, 1, "linked.bin"), (27, 1, "linked.bin")], 7000),
            ),
            (42, file(1, &[(20, 1, "gone.txt")], 10)),
            (43, dir(1, (20, 1), "old")),
            (44, file(1, &[(43, 1, "x")], 20)),
            (45, dir(1, (24, 1), "moving")),
            (46, file(1, &[(45, 1, "m")], 999)),
        ] {
            before.insert(number, fresh);
        }
        let mut after = before.clone();
        // A file grows past its folder's neighbours; a manifest makes
        // `target` build output; `HEAD` makes `repo` a git store; `src`
        // outgrows `.cache`, so `mystery` is code now.
        after.insert(23, file(1, &[(22, 1, "out.bin")], 50_000));
        after.insert(50, file(1, &[(21, 1, "Cargo.toml")], 1));
        after.insert(51, file(1, &[(24, 1, "HEAD")], 1));
        after.insert(61, file(1, &[(60, 1, "small")], 90_000));
        // Gone, renamed, moved with what it holds, one name fewer.
        after.remove(&43);
        after.remove(&44);
        after.insert(40, file(1, &[(5, 5, "notes.md")], 300));
        after.insert(45, dir(1, (20, 1), "moving"));
        after.insert(41, file(1, &[(20, 1, "linked.bin")], 7000));
        // Made since: a folder with a file, and a record reused.
        after.insert(52, dir(1, (20, 1), "fresh"));
        after.insert(53, file(1, &[(52, 1, "f")], 64));
        after.insert(42, dir(2, root, "reborn"));
        after.insert(54, file(1, &[(42, 2, "inside")], 4097));

        for options in [
            exact(),
            ScanOptions {
                metric: Metric::Files,
                apparent_size: true,
                ..exact()
            },
        ] {
            let mut tree = built(&before, &options);
            patch(&mut tree, &before, &after, &options).expect("patched");
            let expected = built(&after, &options);
            assert_eq!(lines(&tree), lines(&expected));
            // And again from what a save keeps.
            let compact = tree.compact().expect("compact");
            assert_eq!(lines(&compact), lines(&expected));
            assert!(tree.has_garbage());
            assert!(!compact.has_garbage());
            assert!(compact.is_valid());
        }
        let expected = built(&after, &exact());
        let named = |path: &[&str]| {
            let mut node = expected.root();
            for part in path {
                node = node.child_named(part).expect("there");
            }
            node
        };
        assert_eq!(
            named(&["src", "rust-thing", "target", "out.bin"]).reclaim(),
            Some(Reclaim::BuildOutput)
        );
        assert_eq!(named(&["repo", "refs"]).category(), Category::Git);
        assert_eq!(named(&["mystery"]).category(), Category::Code);
    }

    #[test]
    fn hardlinks_counted_once_stay_counted_once_through_a_patch() {
        let root = (ROOT, 5);
        let before: Volume = [
            (20, dir(1, root, "a")),
            (21, dir(1, root, "b")),
            (30, file(1, &[(20, 1, "x"), (21, 1, "x")], 8192)),
            (31, file(1, &[(20, 1, "y")], 4096)),
        ]
        .into_iter()
        .collect();
        let mut after = before.clone();
        after.insert(31, file(1, &[(20, 1, "y"), (21, 1, "y")], 4096));
        after.insert(30, file(1, &[(20, 1, "x"), (21, 1, "x")], 16384));
        let options = ScanOptions::default();
        let mut tree = built(&before, &options);
        patch(&mut tree, &before, &after, &options).expect("patched");
        let root = tree.root();
        assert_eq!((root.bytes(), root.files()), (16384 + 4096, 4));
    }

    #[test]
    fn a_hardlink_charged_name_leaving_the_view_needs_a_whole_read() {
        let root = (ROOT, 5);
        let before: Volume = [
            (20, dir(1, root, "a")),
            (21, dir(1, root, "b")),
            (30, file(1, &[(20, 1, "x"), (21, 1, "x")], 8192)),
        ]
        .into_iter()
        .collect();
        for (dedup, patched) in [(true, false), (false, true)] {
            let options = ScanOptions {
                include_hidden: false,
                dedup_hardlinks: dedup,
                ..ScanOptions::default()
            };
            let mut tree = built(&before, &options);
            // The folder of whichever name the build charged is hidden now;
            // `x` in the other still weighs nothing.
            let a = tree.root().child_named("a").expect("a");
            let (number, name) =
                if a.bytes() > 0 { (20, "a") } else { (21, "b") };
            let mut hidden = dir(1, root, name);
            hidden.info.set_attributes(FILE_ATTRIBUTE_HIDDEN);
            let mut after = before.clone();
            after.insert(number, hidden);
            let outcome = patch(&mut tree, &before, &after, &options);
            assert_eq!(outcome.is_some(), patched, "dedup {dedup}");
        }
    }

    #[test]
    fn a_folder_coming_into_view_with_unknown_entries_needs_a_whole_read() {
        let root = (ROOT, 5);
        let options = ScanOptions {
            include_hidden: false,
            ..exact()
        };
        for attributes in [EVICTED, FILE_ATTRIBUTE_HIDDEN] {
            let mut away = dir(1, root, "cloud");
            away.info.set_attributes(attributes);
            let before: Volume = [
                (20, away),
                (21, file(1, &[(20, 1, "held")], 100)),
                (22, dir(1, (20, 1), "inner")),
                (23, file(1, &[(22, 1, "deep")], 100)),
            ]
            .into_iter()
            .collect();
            // Made local, or shown: its entries were never in the tree.
            let mut shown = before.clone();
            shown.insert(20, dir(1, root, "cloud"));
            let mut tree = built(&before, &options);
            assert!(patch(&mut tree, &before, &shown, &options).is_none());
            // Nor were those of a folder moved out of it.
            let mut moved = before.clone();
            moved.insert(22, dir(1, root, "inner"));
            let mut tree = built(&before, &options);
            assert!(patch(&mut tree, &before, &moved, &options).is_none());
            // One made since holds only what the journal names.
            let mut made = shown.clone();
            made.insert(20, dir(2, root, "cloud"));
            made.remove(&21);
            made.remove(&22);
            made.remove(&23);
            made.insert(24, file(1, &[(20, 2, "new")], 5));
            let mut tree = built(&before, &options);
            patch(&mut tree, &before, &made, &options).expect("patched");
            assert_eq!(lines(&tree), lines(&built(&made, &options)));
        }
    }

    #[test]
    fn a_tree_deeper_than_a_whole_read_goes_needs_a_whole_read() {
        // Chain `a` is 900 folders deep, chain `b` 300; `b` moves into `a`.
        let root = (ROOT, 5);
        let mut before = Volume::new();
        for (first, depth, name) in [(100, 900, "a"), (2000, 300, "b")] {
            let mut parent = root;
            for number in first..first + depth {
                before.insert(number, dir(1, parent, name));
                parent = (number, 1);
            }
        }
        let options = ScanOptions::default();
        for (holder, fits) in [(799, true), (999, false)] {
            let mut after = before.clone();
            after.insert(2000, dir(1, (holder, 1), "b"));
            let mut tree = built(&before, &options);
            let patched = patch(&mut tree, &before, &after, &options);
            assert_eq!(patched.is_some(), fits, "beneath {holder}");
        }

        // A kept tree that deep is refused with no change at all.
        let chain = |deepest: usize| Tree {
            name: Box::default(),
            dirs: (0..=deepest)
                .map(|index| Dir {
                    first: index as u32,
                    len: u32::from(index < deepest),
                    parent: index.checked_sub(1).map_or(NONE, |up| up as u32),
                    ..Dir::EMPTY
                })
                .collect(),
            segs: vec![Seg {
                items: (1..=deepest)
                    .map(|child| Item {
                        kind: DIRECTORY,
                        value: child as u64,
                        len: 1,
                        ..Item::default()
                    })
                    .collect(),
                text: "d".to_owned(),
            }],
            volumes: vec![0],
        };
        for (deepest, fits) in [(MOST_LEVELS - 1, true), (MOST_LEVELS, false)] {
            let mut tree = chain(deepest);
            assert!(tree.is_valid());
            let patched =
                tree.patch(&[], &FxHashMap::default(), |_| false, &options);
            assert_eq!(patched.is_some(), fits, "{deepest} deep");
        }
    }

    /// A small xorshift: the same volumes on every run.
    struct Random(u64);

    impl Random {
        fn below(&mut self, count: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % count.max(1) as u64) as usize
        }
    }

    /// Names the kinds depend on, and some that mean nothing.
    const NAMES: [&str; 20] = [
        "a",
        "b",
        "src",
        "target",
        "Cargo.toml",
        "objects",
        "refs",
        "HEAD",
        ".cache",
        "node_modules",
        "package.json",
        "logs",
        "Application Support",
        ".microsandbox",
        "snapshots",
        "layers",
        "x.bin",
        "y.txt",
        "Downloads",
        ".git",
    ];

    /// Directories of `volume`: `(record, sequence)`, the root first.
    fn directories(volume: &Volume) -> Vec<(u32, u16)> {
        std::iter::once((ROOT, 5))
            .chain(
                volume
                    .iter()
                    .filter(|(_, fresh)| fresh.info.directory)
                    .map(|(&number, fresh)| (number, fresh.info.sequence)),
            )
            .collect()
    }

    /// Whether `number` is `within` or beneath it.
    fn beneath(volume: &Volume, mut number: u32, within: u32) -> bool {
        for _ in 0..64 {
            if number == within {
                return true;
            }
            match volume.get(&number) {
                Some(fresh) if fresh.info.directory => {
                    number = fresh.names[0].0;
                }
                _ => return false,
            }
        }
        true
    }

    /// A name `parent` does not hold yet.
    fn free_name(
        volume: &Volume,
        parent: u32,
        random: &mut Random,
    ) -> Option<String> {
        let taken = |name: &str| {
            volume.values().any(|fresh| {
                fresh
                    .names
                    .iter()
                    .any(|(p, _, n)| *p == parent && n == name)
            })
        };
        let name = NAMES[random.below(NAMES.len())];
        (!taken(name)).then(|| name.to_owned())
    }

    /// One change as a volume sees them: a file resized, something
    /// renamed, moved, deleted or made, a name linked or unlinked, a
    /// record reused.
    fn change(volume: &mut Volume, random: &mut Random, next: &mut u32) {
        let numbers: Vec<u32> = volume.keys().copied().collect();
        let dirs = directories(volume);
        let pick = numbers.get(random.below(numbers.len())).copied();
        let (parent, parent_sequence) = dirs[random.below(dirs.len())];
        let Some(name) = free_name(volume, parent, random) else {
            return;
        };
        let size = [0, 1, 4096, 5000, 70_000, 1 << 30][random.below(6)];
        match (random.below(9), pick) {
            (0, Some(number)) if !volume[&number].info.directory => {
                let fresh = volume.get_mut(&number).expect("there");
                fresh.info.apparent = size;
                fresh.info.allocated = size.next_multiple_of(4096);
            }
            (1 | 2, Some(number)) if !beneath(volume, parent, number) => {
                let fresh = volume.get_mut(&number).expect("there");
                fresh.names[0] = (parent, parent_sequence, name);
            }
            (3, Some(number)) => {
                // A folder goes with everything in it; a file with another
                // name elsewhere keeps that one.
                let gone: BTreeSet<u32> = volume
                    .iter()
                    .filter(|(_, fresh)| fresh.info.directory)
                    .map(|(&other, _)| other)
                    .filter(|&other| beneath(volume, other, number))
                    .chain([number])
                    .collect();
                volume.retain(|other, fresh| {
                    if gone.contains(other) {
                        return false;
                    }
                    fresh.names.retain(|(parent, ..)| !gone.contains(parent));
                    fresh.info.names = fresh.names.len() as u8;
                    !fresh.names.is_empty()
                });
            }
            (4, _) => {
                volume.insert(
                    *next,
                    file(1, &[(parent, parent_sequence, &name)], size),
                );
                *next += 1;
            }
            (5, _) => {
                volume.insert(*next, dir(1, (parent, parent_sequence), &name));
                *next += 1;
            }
            (6, Some(number)) if !volume[&number].info.directory => {
                let fresh = volume.get_mut(&number).expect("there");
                fresh.names.push((parent, parent_sequence, name));
                fresh.info.names += 1;
            }
            (7, Some(number)) if volume[&number].names.len() > 1 => {
                let fresh = volume.get_mut(&number).expect("there");
                fresh.names.pop();
                fresh.info.names -= 1;
            }
            (8, Some(number)) if !volume[&number].info.directory => {
                let sequence = volume[&number].info.sequence + 1;
                let fresh = if random.below(2) == 0 {
                    dir(sequence, (parent, parent_sequence), &name)
                } else {
                    file(sequence, &[(parent, parent_sequence, &name)], size)
                };
                volume.insert(number, fresh);
            }
            _ => {}
        }
    }

    #[test]
    fn a_chain_of_patched_changes_keeps_what_a_whole_read_would_make() {
        for (seed, options) in [
            (0x2545_F491_4F6C_DD1D, exact()),
            (
                0x9E37_79B9_7F4A_7C15,
                ScanOptions {
                    metric: Metric::Files,
                    ..exact()
                },
            ),
        ] {
            let mut random = Random(seed);
            let mut volume = Volume::new();
            let mut next = 20;
            for _ in 0..40 {
                change(&mut volume, &mut random, &mut next);
            }
            let mut tree = built(&volume, &options);
            for round in 0..300 {
                let before = volume.clone();
                for _ in 0..=random.below(4) {
                    change(&mut volume, &mut random, &mut next);
                }
                patch(&mut tree, &before, &volume, &options)
                    .unwrap_or_else(|| panic!("round {round} patched"));
                assert_eq!(
                    lines(&tree),
                    lines(&built(&volume, &options)),
                    "round {round}"
                );
                // A save now and then, as a resumed scan makes.
                if round % 7 == 0 {
                    tree = tree.compact().expect("compact");
                    assert!(tree.is_valid());
                }
            }
        }
    }
}

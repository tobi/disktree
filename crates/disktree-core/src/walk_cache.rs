//! Folder snapshots validated with the unprivileged NTFS change journal.
//!
//! The saved tree stays separate from the MFT snapshot. A warm scan lists
//! changed parents and every cached alias of a changed file, then settles
//! only those ancestor chains. Keeping the original checkpoint avoids a
//! large cache rewrite on each launch.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::{File, OpenOptions};
use std::hash::{Hash as _, Hasher as _};
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_UNPRIVILEGED_USN_JOURNAL,
    USN_REASON_CLOSE,
};

use super::{Classified, ScanOptions, WalkContext, push_named};
use crate::tree::{
    Dir, FILE, IDENTIFIED, Item, Metric, NONE, READ_ERROR, Seg, Tree, name_in,
    seconds,
};
use crate::windows;

#[path = "walk_cache/store.rs"]
mod store;

const MAX_AGE: u64 = 24 * 60 * 60;
const MAX_CHANGES: usize = 100_000;
const MAX_DIRECTORIES: usize = 10_000;
const MAX_JOURNAL_BYTES: usize = 64 << 20;

// ponytail: this bounds the blind spot without opening millions of files.
// A writer predating the retained journal can still be missed when its
// cached size is below this set; close or the 24-hour full walk repairs it.
const LARGEST_FILES: usize = 1024;
#[derive(Clone, Copy)]
struct Journal {
    id: u64,
    first: u64,
    next: u64,
}

impl Journal {
    const fn covers(self, id: u64, cursor: u64) -> bool {
        self.id == id && self.first <= cursor && cursor <= self.next
    }
}

pub(super) struct Checkpoint {
    root: PathBuf,
    file: PathBuf,
    handle: File,
    volume: u64,
    root_id: u64,
    journal: Journal,
    created: u64,
    options: u64,
}

impl Checkpoint {
    pub(super) fn open(root: &Path, context: &WalkContext) -> Option<Self> {
        let options = &context.options;
        let cache = options.cache.as_ref()?;
        if options.follow_links
            || !options.one_filesystem
            || options.max_depth.is_some()
            || context.known.is_some()
        {
            return None;
        }
        let volume = windows::walk_volume(root)?;
        let volume_root = windows::volume_root(root)?;
        if !windows::file_table_readable(&volume_root) {
            return None;
        }
        let (device, root_id) = windows::identity(root)?;
        if device != volume {
            return None;
        }
        let handle = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(volume_root)
            .ok()?;
        let journal = query(&handle)?;
        let options = options_key(options);
        let mut hash = FxHasher::default();
        root.hash(&mut hash);
        options.hash(&mut hash);
        Some(Self {
            root: root.to_path_buf(),
            file: windows::cache_path(
                cache,
                &format!("walk-{:016x}.bin", hash.finish()),
            ),
            handle,
            volume,
            root_id,
            journal,
            created: now(),
            options,
        })
    }

    pub(super) fn resume(&self, context: &WalkContext) -> Option<Tree> {
        // The last scan's tree may still be on its way to this file.
        crate::scan::wait_for_cache();
        let started = Instant::now();
        let mut state = store::load(&self.file)?;
        if state.root != self.root.to_str()?
            || state.volume != self.volume
            || state.root_id != self.root_id
            || state.options != self.options
            || state.tree.root().inode() != Some((self.volume, self.root_id))
            || !covered(&state, self.journal, self.created)
        {
            return None;
        }
        let loaded = started.elapsed();
        let changes = changes(&self.handle, self.journal, state.next)?;
        let mut changed: FxHashSet<u64> =
            changes.files.keys().copied().collect();
        changed.extend(state.open.iter().copied());
        let open = state
            .open
            .iter()
            .copied()
            .filter(|id| changes.files.get(id).is_none_or(|closed| !closed))
            .chain(
                changes
                    .files
                    .iter()
                    .filter_map(|(&id, &closed)| (!closed).then_some(id)),
            )
            .collect();
        if changed.len() > MAX_CHANGES {
            return None;
        }
        // Just loaded, so nothing else shares it.
        let tree = Arc::get_mut(&mut state.tree)?;
        // What is listed again goes into a segment of its own, after the
        // one the tree loaded into: nothing already there moves.
        let seg = u32::try_from(tree.segs.len()).ok()?;
        tree.segs.push(Seg::default());
        let mut update = Update {
            context,
            changed,
            parents: changes.parents,
            refreshed: FxHashSet::default(),
            touched: FxHashSet::default(),
            listed: 0,
            seg,
        };
        let mut chains = FxHashSet::default();
        let mut pending = FxHashSet::default();
        loop {
            chains.clear();
            pending.clear();
            update.mark(tree, 0, &mut pending, &mut chains)?;
            if pending.is_empty() {
                break;
            }
            if update.refreshed.len() + pending.len() > MAX_DIRECTORIES {
                return None;
            }
            update.refresh(tree, 0, &self.root, &pending, &chains, 0)?;
            if context.cancelled() || update.touched.len() > MAX_CHANGES {
                return None;
            }
        }
        let mut seen = FxHashSet::default();
        update.settle(tree, 0, &mut seen);
        let (current, current_changed) =
            update.refresh_current(tree, &self.root, open)?;
        if !update.refreshed.is_empty() || current_changed {
            // Classification can depend on sibling names and the dominant
            // top-level child. Reuse that policy rather than approximate it.
            crate::classify::classify(tree);
        }
        let after = query(&self.handle)?;
        if !covered(&state, after, now()) || after.next < self.journal.next {
            return None;
        }
        if context.cancelled() {
            return None;
        }
        if std::env::var_os("DISKTREE_WALK_TRACE").is_some() {
            eprintln!(
                "walk-cache warm load_ms={} total_ms={} relisted={} refreshed_files={} current_files={}",
                loaded.as_millis(),
                started.elapsed().as_millis(),
                update.refreshed.len(),
                update.touched.len(),
                current,
            );
        }
        Arc::into_inner(state.tree)
    }

    /// Keep `tree` for the next scan, on a thread of its own: the scan
    /// hands it over meanwhile. A cancelled walk keeps nothing.
    pub(super) fn save(self, tree: &Arc<Tree>, context: &WalkContext) {
        if context.cancelled() {
            trace("cold unavailable: cancelled");
            return;
        }
        let tree = Arc::clone(tree);
        crate::scan::save_later(move || self.write(tree));
    }

    fn write(self, tree: Arc<Tree>) {
        if !cacheable(&tree, self.volume) {
            trace("cold unavailable: unsupported tree entry");
            return;
        }
        let Some(after) = query(&self.handle) else {
            trace("cold unavailable: journal query failed");
            return;
        };
        if !after.covers(self.journal.id, self.journal.next) {
            trace("cold unavailable: journal wrapped during walk");
            return;
        }
        // Only the walk's starting cursor must remain covered; expiry of
        // older history does not lose a change made during this walk. Seed
        // known writers from the history retained now, not an expired start.
        let Some(changes) = changes(&self.handle, after, after.first) else {
            trace("cold unavailable: retained journal unreadable or too large");
            return;
        };
        let Some(root) = self.root.to_str() else {
            return;
        };
        let state = store::State {
            root: root.to_owned(),
            volume: self.volume,
            root_id: self.root_id,
            journal: self.journal.id,
            next: self.journal.next,
            created: self.created,
            options: self.options,
            open: changes
                .files
                .into_iter()
                .filter_map(|(id, closed)| (!closed).then_some(id))
                .collect(),
            tree,
        };
        let result = store::save(&self.file, &state);
        if std::env::var_os("DISKTREE_WALK_TRACE").is_some() {
            eprintln!(
                "walk-cache cold save={result:?} open={}",
                state.open.len()
            );
        }
    }
}

const fn options_key(options: &ScanOptions) -> u64 {
    (options.apparent_size as u64)
        | ((options.include_hidden as u64) << 1)
        | ((options.dedup_hardlinks as u64) << 2)
        | ((matches!(options.metric, Metric::Files) as u64) << 3)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

const fn covered(state: &store::State, journal: Journal, now: u64) -> bool {
    journal.covers(state.journal, state.next)
        && now >= state.created
        && now - state.created <= MAX_AGE
}

fn query(handle: &File) -> Option<Journal> {
    let mut output = [0; 80];
    let len =
        windows::control(handle, FSCTL_QUERY_USN_JOURNAL, &[], &mut output)
            .ok()?;
    let data = output.get(..len)?;
    let journal = Journal {
        id: number(data, 0)?,
        first: number(data, 8)?.max(number(data, 24)?),
        next: number(data, 16)?,
    };
    (journal.first <= journal.next).then_some(journal)
}

struct Changes {
    files: FxHashMap<u64, bool>,
    parents: FxHashSet<u64>,
}

fn changes(handle: &File, journal: Journal, from: u64) -> Option<Changes> {
    if from < journal.first || from > journal.next {
        return None;
    }
    let mut changes = Changes {
        files: FxHashMap::default(),
        parents: FxHashSet::default(),
    };
    let mut buffer = vec![0; 1 << 20];
    let mut cursor = from;
    let mut bytes = 0;
    while cursor < journal.next {
        let mut input = [0_u8; 40];
        input[..8].copy_from_slice(&cursor.to_le_bytes());
        input[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        input[32..40].copy_from_slice(&journal.id.to_le_bytes());
        let len = windows::control(
            handle,
            FSCTL_READ_UNPRIVILEGED_USN_JOURNAL,
            &input,
            &mut buffer,
        )
        .ok()?;
        bytes += len;
        if bytes > MAX_JOURNAL_BYTES {
            return None;
        }
        let data = buffer.get(..len)?;
        let next = number(data, 0)?;
        if next <= cursor {
            return None;
        }
        parse_changes(data.get(8..)?, cursor, next, &mut changes)?;
        cursor = next;
    }
    let after = query(handle)?;
    if after.id != journal.id || after.first > from || after.next < cursor {
        return None;
    }
    Some(changes)
}

fn parse_changes(
    mut bytes: &[u8],
    from: u64,
    next: u64,
    changes: &mut Changes,
) -> Option<()> {
    let mut last = None;
    while !bytes.is_empty() {
        let length =
            u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
        let version = u16::from_le_bytes(bytes.get(4..6)?.try_into().ok()?);
        if length < 60 || !length.is_multiple_of(8) || version != 2 {
            return None;
        }
        let record = bytes.get(..length)?;
        let usn = number(record, 24)?;
        if usn < from || usn >= next || last.is_some_and(|last| usn <= last) {
            return None;
        }
        last = Some(usn);
        let id = number(record, 8)?;
        let parent = number(record, 16)?;
        let reason = u32::from_le_bytes(record.get(40..44)?.try_into().ok()?);
        changes.files.insert(id, reason & USN_REASON_CLOSE != 0);
        changes.parents.insert(parent);
        if changes.files.len() > MAX_CHANGES
            || changes.parents.len() > MAX_CHANGES
        {
            return None;
        }
        bytes = bytes.get(length..)?;
    }
    Some(())
}

fn number(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

fn trace(message: &str) {
    if std::env::var_os("DISKTREE_WALK_TRACE").is_some() {
        eprintln!("walk-cache {message}");
    }
}

/// A directory's file number, where the tree keeps one.
const fn number_of(dir: &Dir) -> Option<u64> {
    if dir.identified() { Some(dir.id) } else { None }
}

/// A directory's identity, device and number.
fn identity_of(tree: &Tree, dir: &Dir) -> Option<(u64, u64)> {
    Some((tree.volume(dir.volume), number_of(dir)?))
}

/// An entry's identity, device and number.
fn item_identity(tree: &Tree, item: &Item) -> Option<(u64, u64)> {
    item.identified()
        .then(|| (tree.volume(item.volume), item.id))
}

/// Whether the tree can be kept: every directory and file has its number
/// on `volume`, and nothing is deeper than a kept tree may be.
fn cacheable(tree: &Tree, volume: u64) -> bool {
    let known = |identity: Option<(u64, u64)>| {
        identity.is_some_and(|(device, id)| device == volume && id != 0)
    };
    let mut stack = vec![(0_u32, 0_usize)];
    let mut met = 0;
    while let Some((index, depth)) = stack.pop() {
        met += 1;
        if depth > 512 || met > tree.dirs.len() {
            trace("cold unavailable: tree exceeds cache depth limit");
            return false;
        }
        let Some(dir) = tree.dirs.get(index as usize) else {
            return false;
        };
        if !known(identity_of(tree, dir)) {
            trace("cold unavailable: a directory without an identity");
            return false;
        }
        for item in tree.run(dir) {
            if item.is_dir() {
                stack.push((item.value as u32, depth + 1));
            } else if item.kind == FILE && !known(item_identity(tree, item)) {
                trace("cold unavailable: a file without an identity");
                return false;
            }
        }
    }
    true
}

struct CurrentFile {
    bytes: u64,
    modified: i64,
    charged: bool,
}

/// The [`LARGEST_FILES`] largest files the tree shows, as `(size, id)`.
fn largest_files(tree: &Tree, largest: &mut BinaryHeap<Reverse<(u64, u64)>>) {
    let mut stack = vec![0_u32];
    while let Some(index) = stack.pop() {
        let Some(dir) = tree.dirs.get(index as usize) else {
            continue;
        };
        for item in tree.run(dir) {
            if item.is_dir() {
                stack.push(item.value as u32);
            } else if item.kind == FILE && item.identified() {
                let candidate = Reverse((item.value, item.id));
                if largest.len() < LARGEST_FILES {
                    largest.push(candidate);
                } else if let Some(mut smallest) = largest.peek_mut()
                    && candidate < *smallest
                {
                    *smallest = candidate;
                }
            }
        }
    }
}

fn mark_current(
    tree: &Tree,
    index: u32,
    current: &FxHashMap<u64, Option<CurrentFile>>,
    chains: &mut FxHashSet<u64>,
) -> Option<bool> {
    let dir = tree.dirs.get(index as usize)?;
    let mut needed = false;
    for item in tree.run(dir) {
        needed |= if item.is_dir() {
            mark_current(tree, item.value as u32, current, chains)?
        } else {
            item.kind == FILE
                && item.identified()
                && current.contains_key(&item.id)
        };
    }
    if needed {
        chains.insert(number_of(dir)?);
    }
    Some(needed)
}

struct Update<'a> {
    context: &'a WalkContext,
    changed: FxHashSet<u64>,
    parents: FxHashSet<u64>,
    refreshed: FxHashSet<u64>,
    touched: FxHashSet<u64>,
    listed: usize,
    /// The segment runs listed again go into.
    seg: u32,
}

/// A directory beneath one being refreshed that a change chain enters:
/// its index, and what finds it on disk.
struct Chained {
    index: u32,
    name: String,
    identity: Option<(u64, u64)>,
}

impl Update<'_> {
    fn mark(
        &self,
        tree: &Tree,
        index: u32,
        pending: &mut FxHashSet<u64>,
        chains: &mut FxHashSet<u64>,
    ) -> Option<bool> {
        let dir = tree.dirs.get(index as usize)?;
        let id = number_of(dir)?;
        let mut dirty = !self.refreshed.contains(&id)
            && (dir.flags & READ_ERROR != 0
                || self.parents.contains(&id)
                || self.changed.contains(&id));
        let mut below = false;
        for item in tree.run(dir) {
            if item.is_dir() {
                below |= self.mark(tree, item.value as u32, pending, chains)?;
            } else if !self.refreshed.contains(&id)
                && item.identified()
                && (self.changed.contains(&item.id)
                    || self.touched.contains(&item.id))
            {
                dirty = true;
            }
        }
        if dirty {
            pending.insert(id);
        }
        if dirty || below {
            chains.insert(id);
        }
        Some(dirty || below)
    }

    /// The directories beneath `dir` that `chains` holds.
    fn chained(
        tree: &Tree,
        index: u32,
        chains: &FxHashSet<u64>,
    ) -> Option<Vec<Chained>> {
        let dir = tree.dirs.get(index as usize)?;
        Some(
            tree.run(dir)
                .iter()
                .filter(|item| item.is_dir())
                .filter_map(|item| {
                    let child = tree.dir(item.value)?;
                    chains.contains(&number_of(child)?).then(|| Chained {
                        index: item.value as u32,
                        name: tree.text(dir.seg, item).to_owned(),
                        identity: identity_of(tree, child),
                    })
                })
                .collect(),
        )
    }

    fn refresh(
        &mut self,
        tree: &mut Tree,
        index: u32,
        path: &Path,
        pending: &FxHashSet<u64>,
        chains: &FxHashSet<u64>,
        depth: usize,
    ) -> Option<()> {
        let id = number_of(tree.dirs.get(index as usize)?)?;
        if !chains.contains(&id) {
            return Some(());
        }
        if pending.contains(&id) && !self.refreshed.contains(&id) {
            self.relist(tree, index, path, depth)?;
        }
        for child in Self::chained(tree, index, chains)? {
            let path = self.child_path(path, &child.name, child.identity)?;
            self.refresh(tree, child.index, &path, pending, chains, depth + 1)?;
        }
        Some(())
    }

    fn relist(
        &mut self,
        tree: &mut Tree,
        index: u32,
        path: &Path,
        depth: usize,
    ) -> Option<()> {
        if depth > 512
            || self.context.cancelled()
            || self.refreshed.len() >= MAX_DIRECTORIES
        {
            return None;
        }
        let dir = *tree.dirs.get(index as usize)?;
        let key = identity_of(tree, &dir)?;
        self.refreshed.insert(key.1);
        let mut old = FxHashMap::default();
        for item in tree.run(&dir) {
            if item.is_dir() {
                if let Some(child) = tree.dir(item.value) {
                    old.insert(identity_of(tree, child), item.value as u32);
                }
            } else if item.identified() {
                self.touched.insert(item.id);
            }
        }
        let mut read_error = false;
        let mut run = Vec::new();
        let mut text = String::new();
        match super::list(path, self.context.volume.get().copied()) {
            Ok(entries) => {
                if entries.identity()? != key {
                    return None;
                }
                for entry in entries {
                    self.listed += 1;
                    if self.listed > MAX_CHANGES || self.context.cancelled() {
                        return None;
                    }
                    let entry = entry.ok()?;
                    match self.context.classify(path, &entry) {
                        Classified::Subdirectory { path, inode } => {
                            let child = if let Some(child) = old.remove(&inode)
                            {
                                child
                            } else {
                                let child =
                                    u32::try_from(tree.dirs.len()).ok()?;
                                let mut made = Dir {
                                    seg: self.seg,
                                    ..Dir::EMPTY
                                };
                                if let Some((volume, id)) = inode {
                                    made.id = id;
                                    made.volume = tree.intern(volume)?;
                                    made.flags = IDENTIFIED;
                                }
                                tree.dirs.push(made);
                                // New trees share cancellation, policy and
                                // limits with this refresh, rather than
                                // start another scan.
                                self.relist(tree, child, &path, depth + 1)?;
                                child
                            };
                            tree.dirs[child as usize].parent = index;
                            let item = Item {
                                kind: crate::tree::DIRECTORY,
                                value: u64::from(child),
                                ..Item::default()
                            };
                            push_named(&mut run, &mut text, item, entry.name());
                        }
                        Classified::Entry(leaf) => {
                            let item = leaf.item(|volume| tree.intern(volume));
                            if item.identified() {
                                self.touched.insert(item.id);
                            }
                            push_named(&mut run, &mut text, item, entry.name());
                        }
                        // A walk that reuses an earlier subtree keeps
                        // nothing: see `Checkpoint::open`.
                        Classified::Known | Classified::Skipped => {}
                        Classified::Unreadable => read_error = true,
                    }
                }
            }
            Err(error) => {
                read_error = true;
                self.context.progress.record_error(path, &error);
            }
        }
        let into = tree.segs.get_mut(self.seg as usize)?;
        let first = u32::try_from(into.items.len()).ok()?;
        for item in &run {
            let mut item = *item;
            let name = name_in(&text, &item);
            item.at = u32::try_from(into.text.len()).ok()?;
            into.text.push_str(name);
            into.items.push(item);
        }
        let dir = &mut tree.dirs[index as usize];
        dir.seg = self.seg;
        dir.first = first;
        dir.len = u32::try_from(run.len()).ok()?;
        if read_error {
            dir.flags |= READ_ERROR;
        } else {
            dir.flags &= !READ_ERROR;
        }
        // Removing the charged name must refresh every remaining alias.
        for &removed in old.values() {
            self.drop_removed(tree, removed)?;
        }
        Some(())
    }

    fn child_path(
        &self,
        parent: &Path,
        name: &str,
        identity: Option<(u64, u64)>,
    ) -> Option<PathBuf> {
        if !name.contains('\u{fffd}') {
            return Some(parent.join(name));
        }
        // Display names lose unpaired UTF-16 surrogates. Recover the native
        // name by identity only when a changed chain must enter that folder.
        super::list(parent, self.context.volume.get().copied())
            .ok()?
            .filter_map(Result::ok)
            .find(|entry| entry.identity() == identity)
            .map(|entry| entry.path(parent))
    }

    /// Drop directory `index`, gone from its parent, and all beneath it,
    /// as [`Dir::parent`] says a dropped one is, touching every file it
    /// held.
    fn drop_removed(&mut self, tree: &mut Tree, index: u32) -> Option<()> {
        let mut stack = vec![index];
        while let Some(index) = stack.pop() {
            let Some(&dir) = tree.dirs.get(index as usize) else {
                continue;
            };
            for item in tree.run(&dir) {
                if item.is_dir() {
                    stack.push(item.value as u32);
                } else if item.identified() {
                    self.touched.insert(item.id);
                    if self.touched.len() > MAX_CHANGES {
                        return None;
                    }
                }
            }
            tree.dirs[index as usize] = Dir {
                parent: NONE,
                len: 0,
                ..dir
            };
        }
        Some(())
    }

    fn refresh_current(
        &self,
        tree: &mut Tree,
        root: &Path,
        open: FxHashSet<u64>,
    ) -> Option<(usize, bool)> {
        let mut largest = BinaryHeap::with_capacity(LARGEST_FILES);
        largest_files(tree, &mut largest);
        let mut current: FxHashMap<u64, Option<CurrentFile>> = open
            .into_iter()
            .chain(largest.into_iter().map(|Reverse((_, id))| id))
            .map(|id| (id, None))
            .collect();
        let mut chains = FxHashSet::default();
        mark_current(tree, 0, &current, &mut chains)?;
        let changed =
            self.measure_current(tree, 0, root, &mut current, &chains)?;
        Some((
            current.values().filter(|value| value.is_some()).count(),
            changed,
        ))
    }

    fn measure_current(
        &self,
        tree: &mut Tree,
        index: u32,
        path: &Path,
        current: &mut FxHashMap<u64, Option<CurrentFile>>,
        chains: &FxHashSet<u64>,
    ) -> Option<bool> {
        let dir = *tree.dirs.get(index as usize)?;
        if !chains.contains(&number_of(&dir)?) {
            return Some(false);
        }

        let mut changed = false;
        for at in 0..dir.len as usize {
            if self.context.cancelled() {
                return None;
            }
            let item = *tree.run(&dir).get(at)?;
            let name = tree.text(dir.seg, &item).to_owned();
            if item.is_dir() {
                let child = tree.dir(item.value)?;
                if number_of(child).is_some_and(|id| chains.contains(&id)) {
                    let path =
                        self.child_path(path, &name, identity_of(tree, child))?;
                    changed |= self.measure_current(
                        tree,
                        item.value as u32,
                        &path,
                        current,
                        chains,
                    )?;
                }
            } else if item.kind == FILE
                && let Some(key) = item_identity(tree, &item)
                && let Some(value) = current.get_mut(&key.1)
            {
                if value.is_none() {
                    let native = self.child_path(path, &name, Some(key))?;
                    let (bytes, modified) = windows::current_file(
                        &native,
                        key,
                        self.context.options.apparent_size,
                    )
                    .or_else(|| {
                        // A file may refuse opens while its parent still
                        // reports the same facts a full walk would read.
                        super::list(path, self.context.volume.get().copied())
                            .ok()?
                            .filter_map(Result::ok)
                            .find(|entry| entry.identity() == Some(key))
                            .map(|entry| {
                                (
                                    if self.context.options.apparent_size {
                                        entry.apparent()
                                    } else {
                                        entry.allocated()
                                    },
                                    entry.modified(),
                                )
                            })
                    })?;
                    *value = Some(CurrentFile {
                        bytes,
                        modified,
                        charged: false,
                    });
                }
                let value = value.as_mut()?;
                let bytes =
                    if self.context.options.dedup_hardlinks && value.charged {
                        0
                    } else {
                        value.bytes
                    };
                value.charged = true;
                let modified = seconds(value.modified);
                changed |= item.value != bytes || item.modified != modified;
                let slot = tree
                    .segs
                    .get_mut(dir.seg as usize)?
                    .items
                    .get_mut(dir.first as usize + at)?;
                slot.value = bytes;
                slot.modified = modified;
            }
        }
        if changed {
            tree.settle(index, self.context.options.metric);
        }
        Some(changed)
    }

    fn settle(
        &self,
        tree: &mut Tree,
        index: u32,
        seen: &mut FxHashSet<u64>,
    ) -> bool {
        let Some(dir) = tree.dirs.get(index as usize).copied() else {
            return false;
        };
        let mut dirty =
            number_of(&dir).is_some_and(|id| self.refreshed.contains(&id));
        for at in 0..dir.len as usize {
            let Some(item) = tree.run(&dir).get(at).copied() else {
                break;
            };
            if item.is_dir() {
                dirty |= self.settle(tree, item.value as u32, seen);
            } else if item.identified()
                && self.touched.contains(&item.id)
                && self.context.options.dedup_hardlinks
                && !seen.insert(item.id)
                && let Some(slot) = tree
                    .segs
                    .get_mut(dir.seg as usize)
                    .and_then(|seg| seg.items.get_mut(dir.first as usize + at))
            {
                slot.value = 0;
            }
        }
        if dirty {
            tree.settle(index, self.context.options.metric);
        }
        dirty
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Draft, NodeKind};

    #[test]
    fn live_refresh_selects_the_largest_files_without_directories() {
        let mut root = Draft::directory("root");
        root.inode = Some((1, 9_999));
        for size in 1..=2_048 {
            let mut node =
                Draft::entry(format!("{size}"), NodeKind::File, size);
            node.inode = Some((1, size));
            root.children.push(node);
        }
        let root = Tree::from_draft(root, Metric::Bytes);
        let mut largest = BinaryHeap::new();
        largest_files(&root, &mut largest);
        let mut sizes: Vec<_> =
            largest.into_iter().map(|Reverse((size, _))| size).collect();
        sizes.sort_unstable();
        assert_eq!(sizes.len(), 1_024);
        assert_eq!(sizes.first(), Some(&1_025));
        assert_eq!(sizes.last(), Some(&2_048));
    }

    #[test]
    fn journal_coverage_refuses_gaps_replacement_and_old_snapshots() {
        let mut state = store::State {
            root: "fixture".into(),
            volume: 1,
            root_id: 2,
            journal: 3,
            next: 100,
            created: 1_000,
            options: 0,
            open: Vec::new(),
            tree: Arc::default(),
        };
        let journal = Journal {
            id: 3,
            first: 100,
            next: 200,
        };
        assert!(covered(&state, journal, 1_000 + MAX_AGE));
        assert!(!covered(&state, journal, 1_001 + MAX_AGE));
        assert!(!covered(&state, journal, 999));
        assert!(!covered(
            &state,
            Journal {
                first: 101,
                ..journal
            },
            1_001
        ));
        assert!(!covered(&state, Journal { id: 4, ..journal }, 1_001));
        state.next = 201;
        assert!(!covered(&state, journal, 1_001));
    }

    #[test]
    fn journal_records_keep_full_references_and_refuse_a_stalled_cursor() {
        let mut record = [0_u8; 64];
        record[..4].copy_from_slice(&64_u32.to_le_bytes());
        record[4..6].copy_from_slice(&2_u16.to_le_bytes());
        let id = (0x11_u64 << 48) | 0x2a;
        record[8..16].copy_from_slice(&id.to_le_bytes());
        record[16..24].copy_from_slice(&13_u64.to_le_bytes());
        record[24..32].copy_from_slice(&100_u64.to_le_bytes());
        record[40..44].copy_from_slice(&USN_REASON_CLOSE.to_le_bytes());
        let mut result = Changes {
            files: FxHashMap::default(),
            parents: FxHashSet::default(),
        };
        assert!(parse_changes(&record, 100, 164, &mut result).is_some());
        assert_eq!(result.files.get(&id), Some(&true));
        assert!(result.parents.contains(&13));
        assert!(parse_changes(&record, 101, 164, &mut result).is_none());
        assert!(parse_changes(&record, 100, 100, &mut result).is_none());
        assert!(parse_changes(&record[..63], 100, 164, &mut result).is_none());
        record[4..6].copy_from_slice(&3_u16.to_le_bytes());
        assert!(parse_changes(&record, 100, 164, &mut result).is_none());
    }
}

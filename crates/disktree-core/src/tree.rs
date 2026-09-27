//! The scanned tree.
//!
//! Flat, not a node per entry: every directory is a [`Dir`], its entries a
//! run of [`Item`]s in the order the tree shows them, and every name sits
//! in a text arena. A node is then 32 bytes and its name, where a boxed
//! node with a name of its own took 120 bytes and a heap block besides:
//! a 4.9 million entry disk is a few hundred megabytes, not a gigabyte.
//! The runs live in segments: a walk fills one per thread as directories
//! finish, and a tree kept on disk is one read back whole. [`Node`] is a
//! view of one entry, for everything that only reads.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use rayon::prelude::*;
use rustc_hash::FxHashSet;

use crate::classify::{Category, Reclaim};

/// What a node represents on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Directory,
    File,
    Symlink,
    /// Sockets, fifos and devices: addressable, but not space.
    Other,
}

impl NodeKind {
    pub const fn is_dir(self) -> bool {
        matches!(self, Self::Directory)
    }

    /// Its [`Item::kind`].
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::File => FILE,
            Self::Symlink => LINK,
            Self::Directory => DIRECTORY,
            Self::Other => OTHER,
        }
    }

    const fn of(code: u8) -> Self {
        match code {
            FILE => Self::File,
            LINK => Self::Symlink,
            DIRECTORY => Self::Directory,
            _ => Self::Other,
        }
    }
}

/// How a node's importance is measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Metric {
    /// Bytes, apparent or on-disk depending on [`crate::scan::ScanOptions`].
    #[default]
    Bytes,
    /// Number of files at or beneath the node.
    Files,
}

impl Metric {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Bytes => "size",
            Self::Files => "files",
        }
    }

    #[must_use]
    pub const fn toggled(self) -> Self {
        match self {
            Self::Bytes => Self::Files,
            Self::Files => Self::Bytes,
        }
    }
}

/// What an [`Item`] is.
pub(crate) const FILE: u8 = 0;
pub(crate) const LINK: u8 = 1;
pub(crate) const DIRECTORY: u8 = 2;
pub(crate) const OTHER: u8 = 3;

/// No directory: the root's parent, and a directory dropped from a tree.
pub(crate) const NONE: u32 = u32::MAX;

/// A flag of an [`Item`] or a [`Dir`]: its `id` and `volume` are the
/// file's identity, kept for hardlinks and for finding it again.
pub(crate) const IDENTIFIED: u8 = 1;
/// A flag of a [`Dir`]: it could not be read; its contents are unknown.
pub(crate) const READ_ERROR: u8 = 2;

/// A directory's kind not decided yet: see [`code`].
pub(crate) const UNSET: u8 = u8::MAX;

/// One entry of a directory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Item {
    /// The file's number on its volume: an inode, a file id, or for the
    /// file table reader its record with the record's sequence number in
    /// the top 16 bits, as NTFS writes a reference.
    pub id: u64,
    /// A file's size as charged; a directory's index among the dirs.
    pub value: u64,
    /// Last write, in Unix seconds; `0` when unknown.
    pub modified: u32,
    /// Its name: `len` bytes at `at` in its directory's segment's text.
    pub at: u32,
    pub len: u16,
    /// Where `id` is: an index into the tree's volumes.
    pub volume: u16,
    pub kind: u8,
    /// [`IDENTIFIED`].
    pub flags: u8,
}

impl Item {
    pub(crate) const fn is_dir(&self) -> bool {
        self.kind == DIRECTORY
    }

    /// What a file or link weighs in its parent's order.
    pub(crate) const fn key(&self, metric: Metric) -> u64 {
        match metric {
            Metric::Bytes => self.value,
            Metric::Files => (self.kind == FILE) as u64,
        }
    }

    /// A file's or link's totals.
    pub(crate) const fn totals(&self) -> Totals {
        Totals {
            bytes: self.value,
            files: (self.kind == FILE) as u64,
            dirs: 0,
            modified: self.modified,
        }
    }

    pub(crate) const fn identified(&self) -> bool {
        self.flags & IDENTIFIED != 0
    }
}

/// A directory of the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Dir {
    /// Its identity, as an [`Item`]'s.
    pub id: u64,
    /// Totals as its node has them.
    pub bytes: u64,
    pub files: u64,
    pub dirs: u32,
    /// Its entries: `segs[seg].items[first..first + len]`, in the order
    /// shown.
    pub first: u32,
    pub len: u32,
    pub seg: u32,
    /// The directory holding it; [`NONE`] for the root, and for one
    /// dropped from the tree.
    pub parent: u32,
    pub modified: u32,
    pub volume: u16,
    /// Its [`Category`] and [`Reclaim`], by number: see [`code`].
    pub category: u8,
    pub reclaim: u8,
    /// [`IDENTIFIED`] and [`READ_ERROR`].
    pub flags: u8,
}

impl Default for Dir {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl Dir {
    /// A directory with nothing in it, its kind not decided yet.
    pub(crate) const EMPTY: Self = Self {
        id: 0,
        bytes: 0,
        files: 0,
        dirs: 1,
        first: 0,
        len: 0,
        seg: 0,
        parent: NONE,
        modified: 0,
        volume: 0,
        category: UNSET,
        reclaim: UNSET,
        flags: 0,
    };

    /// What this directory weighs in its parent's order.
    pub(crate) const fn key(&self, metric: Metric) -> u64 {
        match metric {
            Metric::Bytes => self.bytes,
            Metric::Files => self.files,
        }
    }

    pub(crate) const fn totals(&self) -> Totals {
        Totals {
            bytes: self.bytes,
            files: self.files,
            dirs: self.dirs,
            modified: self.modified,
        }
    }

    pub(crate) const fn set_totals(&mut self, totals: Totals) {
        self.bytes = totals.bytes;
        self.files = totals.files;
        self.dirs = totals.dirs;
        self.modified = totals.modified;
    }

    pub(crate) const fn identified(&self) -> bool {
        self.flags & IDENTIFIED != 0
    }
}

/// What an entry adds to the directory holding it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Totals {
    pub bytes: u64,
    pub files: u64,
    pub dirs: u32,
    pub modified: u32,
}

impl Totals {
    /// A directory's own share: itself, and nothing in it.
    pub(crate) const DIRECTORY: Self = Self {
        bytes: 0,
        files: 0,
        dirs: 1,
        modified: 0,
    };

    /// Saturating: a corrupt volume's file table can claim any size.
    pub(crate) fn add(&mut self, other: Self) {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.files = self.files.saturating_add(other.files);
        self.dirs = self.dirs.saturating_add(other.dirs);
        self.modified = self.modified.max(other.modified);
    }
}

/// A run of entries and their names: part of a tree, or all of one.
#[derive(Clone, Debug, Default)]
pub(crate) struct Seg {
    pub items: Vec<Item>,
    pub text: String,
}

/// The tree a scan found.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    /// The root's name, which no entry holds.
    pub(crate) name: Box<str>,
    /// `dirs[0]` is the root.
    pub(crate) dirs: Vec<Dir>,
    pub(crate) segs: Vec<Seg>,
    /// The devices identities are on, which [`Item::volume`] numbers.
    pub(crate) volumes: Vec<u64>,
}

/// A time as an [`Item`] keeps it: Unix seconds, `0` for one before 1970,
/// which the tree already treats as unknown, and at most 2106.
pub(crate) fn seconds(modified: i64) -> u32 {
    u32::try_from(modified.max(0)).unwrap_or(u32::MAX)
}

/// A directory's [`Category`] and [`Reclaim`] as numbers: the category's
/// place in the legend, `Other` after it, and the reason's place plus one,
/// `0` for none.
pub(crate) fn code(category: Category, reclaim: Option<Reclaim>) -> (u8, u8) {
    let category = Category::LEGEND
        .iter()
        .position(|&known| known == category)
        .unwrap_or(Category::LEGEND.len());
    let reclaim = reclaim
        .and_then(|reclaim| Reclaim::ALL.iter().position(|&r| r == reclaim))
        .map_or(0, |index| index + 1);
    (category as u8, reclaim as u8)
}

/// See [`code`]; anything else, [`UNSET`] among it, is no kind.
pub(crate) fn decode(
    (category, reclaim): (u8, u8),
) -> (Category, Option<Reclaim>) {
    let category = Category::LEGEND
        .get(usize::from(category))
        .copied()
        .unwrap_or(Category::Other);
    let reclaim = reclaim
        .checked_sub(1)
        .and_then(|index| Reclaim::ALL.get(usize::from(index)).copied());
    (category, reclaim)
}

/// Largest first, then by name: the order every run is kept in. The only
/// ties are names that decoded to the same lossy text, whose order does
/// not matter.
pub(crate) fn order(
    (left_key, left_name): (u64, &[u8]),
    (right_key, right_name): (u64, &[u8]),
) -> std::cmp::Ordering {
    right_key
        .cmp(&left_key)
        .then_with(|| left_name.cmp(right_name))
}

/// `item`'s name in `text`; empty when it is not there.
pub(crate) fn name_in<'a>(text: &'a str, item: &Item) -> &'a str {
    let at = item.at as usize;
    text.get(at..at + usize::from(item.len)).unwrap_or_default()
}

/// Order a run largest first; `dirs` weigh its directories. Unstable
/// where a run is ordered from scratch, since the stable sort's scratch
/// allocation buys nothing; stable where it is ordered again after a
/// change, which takes a run still nearly in order in close to one pass.
fn sort_run(
    run: &mut [Item],
    dirs: &[Dir],
    text: &str,
    metric: Metric,
    stable: bool,
) {
    let key = |item: &Item| {
        if item.is_dir() {
            dirs.get(item.value as usize)
                .map_or(0, |dir| dir.key(metric))
        } else {
            item.key(metric)
        }
    };
    let compare = |left: &Item, right: &Item| {
        order(
            (key(left), name_in(text, left).as_bytes()),
            (key(right), name_in(text, right).as_bytes()),
        )
    };
    if stable {
        run.sort_by(compare);
    } else {
        run.sort_unstable_by(compare);
    }
}

impl Tree {
    /// The scanned root.
    pub const fn root(&self) -> Node<'_> {
        Node {
            tree: self,
            item: None,
            seg: 0,
            parent: NONE,
        }
    }

    /// The node at `crumbs` beneath the root.
    pub fn resolve(&self, crumbs: &[usize]) -> Option<Node<'_>> {
        self.root().resolve(crumbs)
    }

    /// Call the root something else: a scan names it after the path it
    /// was asked for.
    pub fn rename(&mut self, name: Box<str>) {
        self.name = name;
    }

    pub(crate) fn dir(&self, index: u64) -> Option<&Dir> {
        self.dirs.get(usize::try_from(index).ok()?)
    }

    /// A directory's entries.
    pub(crate) fn run(&self, dir: &Dir) -> &[Item] {
        self.segs
            .get(dir.seg as usize)
            .and_then(|seg| seg.items.get(dir.first as usize..))
            .and_then(|rest| rest.get(..dir.len as usize))
            .unwrap_or_default()
    }

    /// The name of `item`, an entry of a directory in segment `seg`.
    pub(crate) fn text(&self, seg: u32, item: &Item) -> &str {
        self.segs
            .get(seg as usize)
            .map_or("", |seg| name_in(&seg.text, item))
    }

    /// An entry's totals: a file's own, a directory's whole.
    pub(crate) fn totals(&self, item: &Item) -> Totals {
        if item.is_dir() {
            self.dir(item.value)
                .map_or_else(Totals::default, Dir::totals)
        } else {
            item.totals()
        }
    }

    /// The device an identity numbered `volume` is on.
    pub(crate) fn volume(&self, volume: u16) -> u64 {
        self.volumes.get(usize::from(volume)).copied().unwrap_or(0)
    }

    /// Total directory `index` from its entries, which must be totalled,
    /// and order them by `metric`.
    pub(crate) fn settle(&mut self, index: u32, metric: Metric) {
        let Some(&dir) = self.dirs.get(index as usize) else {
            return;
        };
        let mut total = Totals::DIRECTORY;
        for item in self.run(&dir) {
            total.add(self.totals(item));
        }
        self.dirs[index as usize].set_totals(total);
        let Self { dirs, segs, .. } = self;
        let Some(seg) = segs.get_mut(dir.seg as usize) else {
            return;
        };
        let start = dir.first as usize;
        if let Some(run) = seg.items.get_mut(start..start + dir.len as usize) {
            sort_run(run, dirs, &seg.text, metric, true);
        }
    }

    /// Order every directory's entries by `metric`, largest first: the
    /// totals stay what they are, only the order changes.
    pub fn reorder(&mut self, metric: Metric) {
        let Self { dirs, segs, .. } = self;
        let dirs: &[Dir] = dirs;
        // Each run where it lies, so a segment splits into runs that can
        // be ordered at once: nothing else can hand out disjoint pieces.
        let mut runs: Vec<(u32, u32, u32)> = dirs
            .par_iter()
            .filter(|dir| dir.len > 1)
            .map(|dir| (dir.seg, dir.first, dir.len))
            .collect();
        runs.par_sort_unstable();
        segs.par_iter_mut().enumerate().for_each(|(index, seg)| {
            let index = index as u32;
            let start = runs.partition_point(|&(seg, ..)| seg < index);
            let end = runs.partition_point(|&(seg, ..)| seg <= index);
            let Seg { items, text } = seg;
            let text: &str = text;
            let mut rest: &mut [Item] = items;
            let mut offset = 0_usize;
            let mut pieces = Vec::with_capacity(end - start);
            for &(_, first, len) in &runs[start..end] {
                let (first, len) = (first as usize, len as usize);
                // Overlapping runs are no tree; order what can be.
                let Some(skip) = first.checked_sub(offset) else {
                    continue;
                };
                if skip.saturating_add(len) > rest.len() {
                    break;
                }
                let (_, tail) = std::mem::take(&mut rest).split_at_mut(skip);
                let (run, tail) = tail.split_at_mut(len);
                pieces.push(run);
                rest = tail;
                offset = first + len;
            }
            pieces.into_par_iter().for_each(|run| {
                sort_run(run, dirs, text, metric, false);
            });
        });
    }

    /// A tree made by hand: totalled and ordered by `metric`, with the
    /// kinds `root` gives its directories.
    pub fn from_draft(root: Draft, metric: Metric) -> Self {
        let mut tree = Self {
            name: root.name.clone(),
            segs: vec![Seg::default()],
            ..Self::default()
        };
        tree.add_draft(root, NONE, metric);
        tree
    }

    /// `volume`'s number among the tree's devices, added if it is new;
    /// `None` past what an entry can number.
    pub(crate) fn intern(&mut self, volume: u64) -> Option<u16> {
        let index = if let Some(index) =
            self.volumes.iter().position(|&v| v == volume)
        {
            index
        } else {
            self.volumes.push(volume);
            self.volumes.len() - 1
        };
        u16::try_from(index).ok()
    }

    fn add_draft(&mut self, draft: Draft, parent: u32, metric: Metric) -> u32 {
        let index = self.dirs.len() as u32;
        let (category, reclaim) = code(draft.category, draft.reclaim);
        let mut dir = Dir {
            parent,
            category,
            reclaim,
            flags: if draft.read_error { READ_ERROR } else { 0 },
            ..Dir::EMPTY
        };
        if let Some((volume, id)) = draft.inode
            && let Some(volume) = self.intern(volume)
        {
            dir.id = id;
            dir.volume = volume;
            dir.flags |= IDENTIFIED;
        }
        self.dirs.push(dir);
        let mut run = Vec::with_capacity(draft.children.len());
        for child in draft.children {
            let mut item = Item {
                kind: child.kind.code(),
                modified: seconds(child.modified),
                value: child.bytes,
                ..Item::default()
            };
            if let Some((volume, id)) = child.inode
                && let Some(volume) = self.intern(volume)
            {
                item.id = id;
                item.volume = volume;
                item.flags = IDENTIFIED;
            }
            let name = child.name.clone();
            if child.kind.is_dir() {
                item.value = u64::from(self.add_draft(child, index, metric));
            }
            run.push((item, name));
        }
        let seg = &mut self.segs[0];
        let first = seg.items.len() as u32;
        for (mut item, name) in run {
            item.at = seg.text.len() as u32;
            item.len = name.len() as u16;
            seg.text.push_str(&name);
            seg.items.push(item);
        }
        let dir = &mut self.dirs[index as usize];
        dir.first = first;
        dir.len = seg.items.len() as u32 - first;
        self.settle(index, metric);
        index
    }
}

/// One node of a [`Tree`], to read: the root, or an entry beneath it.
#[derive(Clone, Copy)]
pub struct Node<'a> {
    tree: &'a Tree,
    /// The entry naming it; `None` for the root.
    item: Option<&'a Item>,
    /// The segment its name is in: its parent's.
    seg: u32,
    /// The directory holding it; [`NONE`] for the root.
    parent: u32,
}

impl std::fmt::Debug for Node<'_> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Node")
            .field("name", &self.name())
            .field("kind", &self.kind())
            .field("bytes", &self.bytes())
            .finish_non_exhaustive()
    }
}

impl<'a> Node<'a> {
    /// Its directory, if it is one.
    fn dir(self) -> Option<&'a Dir> {
        match self.item {
            None => self.tree.dirs.first(),
            Some(item) if item.is_dir() => self.tree.dir(item.value),
            Some(_) => None,
        }
    }

    /// Its entries, or none.
    fn run(self) -> &'a [Item] {
        self.dir().map_or(&[], |dir| self.tree.run(dir))
    }

    pub fn name(self) -> &'a str {
        match self.item {
            None => &self.tree.name,
            Some(item) => self.tree.text(self.seg, item),
        }
    }

    pub fn kind(self) -> NodeKind {
        self.item
            .map_or(NodeKind::Directory, |item| NodeKind::of(item.kind))
    }

    pub fn is_dir(self) -> bool {
        self.kind().is_dir()
    }

    /// Subtree total: direct contents plus every descendant.
    pub fn bytes(self) -> u64 {
        match (self.item, self.dir()) {
            (_, Some(dir)) => dir.bytes,
            (Some(item), None) if !item.is_dir() => item.value,
            _ => 0,
        }
    }

    /// Files at or beneath this node; `1` for a file.
    pub fn files(self) -> u64 {
        match (self.item, self.dir()) {
            (_, Some(dir)) => dir.files,
            (Some(item), None) => u64::from(item.kind == FILE),
            _ => 0,
        }
    }

    /// Directories at or beneath this node; `1` for a directory.
    pub fn dirs(self) -> u64 {
        self.dir().map_or(0, |dir| u64::from(dir.dirs))
    }

    /// Bytes of the leaf entries directly in this directory, or this
    /// file's own size. Summed when asked: only the selection shows it.
    pub fn own_bytes(self) -> u64 {
        if !self.is_dir() {
            return self.bytes();
        }
        self.run()
            .iter()
            .filter(|item| !item.is_dir())
            .fold(0, |sum: u64, item| sum.saturating_add(item.value))
    }

    /// Files directly in this directory; `1` for a file.
    pub fn own_files(self) -> u64 {
        if !self.is_dir() {
            return self.files();
        }
        self.run().iter().filter(|item| item.kind == FILE).count() as u64
    }

    /// Newest write time at or beneath this node, in Unix seconds; `0`
    /// when unknown.
    pub fn modified(self) -> i64 {
        match (self.item, self.dir()) {
            (_, Some(dir)) => i64::from(dir.modified),
            (Some(item), None) => i64::from(item.modified),
            _ => 0,
        }
    }

    /// The directory could not be read; its contents are unknown.
    pub fn read_error(self) -> bool {
        self.dir().is_some_and(|dir| dir.flags & READ_ERROR != 0)
    }

    /// `(device, inode)` where the scan kept one: for hardlinks, and on
    /// Windows for finding a directory again.
    pub fn inode(self) -> Option<(u64, u64)> {
        match (self.item, self.dir()) {
            (_, Some(dir)) => dir
                .identified()
                .then(|| (self.tree.volume(dir.volume), dir.id)),
            (Some(item), None) => item
                .identified()
                .then(|| (self.tree.volume(item.volume), item.id)),
            _ => None,
        }
    }

    /// What kind of data this is, for colour, and why its space can be had
    /// back, if it can: see [`crate::classify`]. A file is what holds it,
    /// or beneath the root what its name says.
    pub fn kinds(self) -> (Category, Option<Reclaim>) {
        if let Some(dir) = self.dir() {
            return decode((dir.category, dir.reclaim));
        }
        if self.parent == 0 {
            return crate::classify::top_level_kind(
                self.name(),
                false,
                || false,
                || None,
                |_| false,
            );
        }
        self.tree
            .dirs
            .get(self.parent as usize)
            .map_or((Category::Other, None), |dir| {
                decode((dir.category, dir.reclaim))
            })
    }

    pub fn category(self) -> Category {
        self.kinds().0
    }

    /// Inherited by everything beneath a reclaimable directory.
    pub fn reclaim(self) -> Option<Reclaim> {
        self.kinds().1
    }

    /// The value a treemap should weight this node by.
    pub fn value(self, metric: Metric) -> u64 {
        match metric {
            Metric::Bytes => self.bytes(),
            Metric::Files => self.files(),
        }
    }

    /// Its children, ordered by [`Metric`] value, descending.
    pub fn children(self) -> Children<'a> {
        let dir = self.dir();
        Children {
            tree: self.tree,
            run: dir.map_or(&[][..], |dir| self.tree.run(dir)).iter(),
            seg: dir.map_or(0, |dir| dir.seg),
            parent: match self.item {
                None => 0,
                Some(item) => item.value as u32,
            },
        }
    }

    pub fn child_count(self) -> usize {
        self.run().len()
    }

    pub fn has_children(self) -> bool {
        !self.run().is_empty()
    }

    pub fn child(self, index: usize) -> Option<Self> {
        self.children().nth(index)
    }

    /// The child with this name, if there is one.
    pub fn child_named(self, name: &str) -> Option<Self> {
        self.children().find(|child| child.name() == name)
    }

    /// Index of the largest child, used to pick a useful descent target.
    pub fn largest_child(self) -> Option<usize> {
        self.has_children().then_some(0)
    }

    /// Follow `crumbs` from this node. Crumbs are child indices, so they stay
    /// valid across re-sorting only for the tree they were produced from.
    pub fn resolve(self, crumbs: &[usize]) -> Option<Self> {
        let mut node = self;
        for &index in crumbs {
            node = node.child(index)?;
        }
        Some(node)
    }

    /// The chain of nodes ending at `crumbs`, including this node.
    pub fn resolve_chain(self, crumbs: &[usize]) -> Vec<Self> {
        let mut chain = vec![self];
        let mut node = self;
        for &index in crumbs {
            match node.child(index) {
                Some(child) => {
                    chain.push(child);
                    node = child;
                }
                None => break,
            }
        }
        chain
    }

    /// Depth of the deepest descendant.
    pub fn depth(self) -> u32 {
        self.children()
            .map(Self::depth)
            .max()
            .map_or(0, |deepest| deepest + 1)
    }
}

/// A node's children, in order.
#[derive(Clone)]
pub struct Children<'a> {
    tree: &'a Tree,
    run: std::slice::Iter<'a, Item>,
    seg: u32,
    parent: u32,
}

impl std::fmt::Debug for Children<'_> {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Children")
            .field("left", &self.run.len())
            .finish_non_exhaustive()
    }
}

impl<'a> Children<'a> {
    const fn node(&self, item: &'a Item) -> Node<'a> {
        Node {
            tree: self.tree,
            item: Some(item),
            seg: self.seg,
            parent: self.parent,
        }
    }
}

impl<'a> Iterator for Children<'a> {
    type Item = Node<'a>;

    fn next(&mut self) -> Option<Node<'a>> {
        let item = self.run.next()?;
        Some(self.node(item))
    }

    fn nth(&mut self, index: usize) -> Option<Node<'a>> {
        let item = self.run.nth(index)?;
        Some(self.node(item))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.run.size_hint()
    }
}

impl DoubleEndedIterator for Children<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let item = self.run.next_back()?;
        Some(self.node(item))
    }
}

impl ExactSizeIterator for Children<'_> {}

/// A tree written out by hand, for [`Tree::from_draft`]: tests, and tools
/// that make a tree of their own.
#[derive(Clone, Debug)]
pub struct Draft {
    pub name: Box<str>,
    pub kind: NodeKind,
    /// A leaf's own size; a directory's comes from its children.
    pub bytes: u64,
    /// A leaf's last write, in Unix seconds; a directory's comes from its
    /// children.
    pub modified: i64,
    pub inode: Option<(u64, u64)>,
    pub read_error: bool,
    /// A directory's kind; a file's comes from what holds it.
    pub category: Category,
    pub reclaim: Option<Reclaim>,
    pub children: Vec<Self>,
}

impl Draft {
    /// A directory with no children yet.
    #[allow(
        clippy::missing_const_for_fn,
        reason = "`impl Into<Box<str>>` cannot be called in a const fn"
    )]
    pub fn directory(name: impl Into<Box<str>>) -> Self {
        Self::entry(name, NodeKind::Directory, 0)
    }

    /// A leaf entry.
    pub fn entry(
        name: impl Into<Box<str>>,
        kind: NodeKind,
        bytes: u64,
    ) -> Self {
        Self {
            name: name.into(),
            kind,
            bytes,
            modified: 0,
            inode: None,
            read_error: false,
            category: Category::Other,
            reclaim: None,
            children: Vec::new(),
        }
    }
}

/// Builds a tree from many threads at once, a directory at a time: each
/// directory's run goes, once it is ordered, into its thread's segment,
/// which is filled and never grown, and the directory to its thread's
/// list. Nothing is copied into a tree later but the directories, and no
/// lock is shared by every thread for every directory.
#[derive(Debug)]
pub(crate) struct Builder {
    next_dir: AtomicU32,
    /// What each thread fills.
    open: Box<[Mutex<Slot>]>,
    /// Segments filled, and runs too large to share one.
    done: Mutex<Vec<Chunk>>,
    next_seg: AtomicU32,
    /// The first device an identity was on, which is nearly always the
    /// only one, and those after it.
    volume: OnceLock<u64>,
    volumes: Mutex<Vec<u64>>,
}

/// One thread's part of a tree being built: the segment it fills, and
/// the directories it placed, by index.
#[derive(Debug, Default)]
struct Slot {
    chunk: Chunk,
    dirs: Vec<(u32, Dir)>,
}

/// A segment being filled, and its number: [`NONE`] before the first run.
#[derive(Debug)]
struct Chunk {
    seg: u32,
    items: Vec<Item>,
    text: String,
}

impl Default for Chunk {
    fn default() -> Self {
        Self {
            seg: NONE,
            items: Vec::new(),
            text: String::new(),
        }
    }
}

/// Entries in a segment a thread fills: 256 KiB, below where the heap
/// hands out whole pages of its own, so a segment only partly filled
/// gives back what it did not use. A run of an eighth of that or more
/// gets a segment of its own, so what is left unused at a segment's end
/// is at most that.
const CHUNK_ITEMS: usize = 8192;
/// Bytes of names in a segment a thread fills: 24 a name.
const CHUNK_TEXT: usize = 24 * CHUNK_ITEMS;

impl Chunk {
    fn new(seg: u32, items: usize, text: usize) -> Self {
        Self {
            seg,
            items: Vec::with_capacity(items),
            text: String::with_capacity(text),
        }
    }

    /// Whether a run fits; never in a chunk not opened yet, which has no
    /// segment for even an empty run to name.
    const fn fits(&self, items: usize, text: usize) -> bool {
        self.seg != NONE
            && self.items.len() + items <= self.items.capacity()
            && self.text.len() + text <= self.text.capacity()
    }
}

impl Builder {
    /// For about `threads` threads at once.
    pub(crate) fn new(threads: usize) -> Self {
        Self {
            next_dir: AtomicU32::new(0),
            open: std::iter::repeat_with(Mutex::default)
                .take(threads.max(1))
                .collect(),
            done: Mutex::default(),
            next_seg: AtomicU32::new(0),
            volume: OnceLock::new(),
            volumes: Mutex::default(),
        }
    }

    /// Places for `count` more directories, numbered from the one this
    /// returns; `None` past what a tree can number, taking none.
    pub(crate) fn reserve(&self, count: u32) -> Option<u32> {
        self.next_dir
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |first| {
                first.checked_add(count).filter(|&end| end < NONE)
            })
            .ok()
    }

    /// `volume`'s number in the tree; `None` past what an item can number.
    pub(crate) fn volume(&self, volume: u64) -> Option<u16> {
        if *self.volume.get_or_init(|| volume) == volume {
            return Some(0);
        }
        let mut volumes = crate::scan::lock(&self.volumes);
        let index =
            if let Some(index) = volumes.iter().position(|&v| v == volume) {
                index
            } else {
                volumes.push(volume);
                volumes.len() - 1
            };
        drop(volumes);
        u16::try_from(index + 1)
            .ok()
            .filter(|&index| index < u16::MAX)
    }

    /// Directory `index` is `dir`, and its entries are `run`, in order,
    /// with `text` bytes of names between them. `None` for a run no
    /// segment can hold: more entries or name bytes than 32 bits number,
    /// or a name longer than an entry keeps.
    pub(crate) fn place<'n>(
        &self,
        index: u32,
        mut dir: Dir,
        run: impl ExactSizeIterator<Item = (Item, &'n str)>,
        text: usize,
    ) -> Option<()> {
        let count = run.len();
        dir.len = u32::try_from(count).ok()?;
        u32::try_from(text).ok()?;
        let slot = rayon::current_thread_index().unwrap_or(0) % self.open.len();
        if count >= CHUNK_ITEMS / 8 || text >= CHUNK_TEXT / 8 {
            let seg = self.next_seg.fetch_add(1, Ordering::Relaxed);
            let mut chunk = Chunk::new(seg, count, text);
            dir.seg = seg;
            dir.first = 0;
            push(&mut chunk, run)?;
            crate::scan::lock(&self.done).push(chunk);
            crate::scan::lock(&self.open[slot]).dirs.push((index, dir));
            return Some(());
        }
        let mut slot = crate::scan::lock(&self.open[slot]);
        let Slot { chunk, dirs } = &mut *slot;
        if !chunk.fits(count, text) {
            let seg = self.next_seg.fetch_add(1, Ordering::Relaxed);
            let full = std::mem::replace(
                chunk,
                Chunk::new(seg, CHUNK_ITEMS, CHUNK_TEXT),
            );
            if full.items.capacity() > 0 {
                crate::scan::lock(&self.done).push(full);
            }
        }
        dir.seg = chunk.seg;
        dir.first = u32::try_from(chunk.items.len()).ok()?;
        push(chunk, run)?;
        dirs.push((index, dir));
        drop(slot);
        Some(())
    }

    /// The tree, its root named `name`: everything placed so far, which
    /// the builder no longer holds.
    pub(crate) fn finish(&self, name: Box<str>) -> Tree {
        use crate::scan::lock;
        let count = self.next_dir.load(Ordering::Relaxed) as usize;
        let mut dirs = vec![Dir::EMPTY; count];
        let mut segs: Vec<Seg> = std::iter::repeat_with(Seg::default)
            .take(self.next_seg.load(Ordering::Relaxed) as usize)
            .collect();
        let mut chunks = std::mem::take(&mut *lock(&self.done));
        for slot in &self.open {
            let Slot {
                mut chunk,
                dirs: placed,
            } = std::mem::take(&mut *lock(slot));
            for (index, dir) in placed {
                if let Some(at) = dirs.get_mut(index as usize) {
                    *at = dir;
                }
            }
            // Only these can be partly filled.
            chunk.items.shrink_to_fit();
            chunk.text.shrink_to_fit();
            chunks.push(chunk);
        }
        for chunk in chunks {
            if let Some(seg) = segs.get_mut(chunk.seg as usize) {
                *seg = Seg {
                    items: chunk.items,
                    text: chunk.text,
                };
            }
        }
        let mut volumes: Vec<u64> =
            self.volume.get().copied().into_iter().collect();
        volumes.extend(std::mem::take(&mut *lock(&self.volumes)));
        Tree {
            name,
            dirs,
            segs,
            volumes,
        }
    }
}

/// Append `run` to `chunk`, each name after the last; `None` at a name
/// that starts past what 32 bits number, or is longer than 16 bits say.
fn push<'n>(
    chunk: &mut Chunk,
    run: impl Iterator<Item = (Item, &'n str)>,
) -> Option<()> {
    for (mut item, name) in run {
        item.at = u32::try_from(chunk.text.len()).ok()?;
        item.len = u16::try_from(name.len()).ok()?;
        chunk.text.push_str(name);
        chunk.items.push(item);
    }
    Some(())
}

/// Identities a scan has met.
///
/// A hash set of every file on a disk is millions of random writes into a
/// table too big for any cache. Inode and file record numbers are small
/// integers, though, so a key on the first volume met whose whole number
/// is below the bitmap's size gets one bit instead: an 8 MiB bitmap the
/// workers set without locking. The file table reader's record numbers
/// fit, as do most inodes. Anything else goes into the hash set, whole:
/// a Windows file id keeps the record's reuse count in its top 16 bits,
/// and that is what tells a file from an older one on the same record,
/// which a subtree kept from an earlier scan can still hold. A walk of
/// one NTFS volume that kept nothing from before has only current ids,
/// so for it the record alone is the key and every file gets a bit too:
/// the hash set of a home folder's 3.6 million ids was 68 MiB, most of
/// what a cold walk held beyond its tree. The set is sharded for the
/// millions a walk may still need: 4.1 million ids took 150 ms across 64
/// locks and 1.2 s behind one, in a synthetic run.
pub(crate) struct Seen {
    /// The first volume met, which is the scanned root's in all but a scan
    /// that leaves its volume.
    volume: OnceLock<u64>,
    /// Made on the first key that fits: most Unix trees, where only files
    /// with more than one name carry an identity, never need it.
    bits: LazyLock<Box<[AtomicU64]>>,
    rest: Box<[Shard]>,
    /// What of a number the bitmap is keyed by: all of it, or the record
    /// below the reuse count.
    mask: u64,
}

impl std::fmt::Debug for Seen {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Seen").finish_non_exhaustive()
    }
}

/// One lock's part of the hash set.
type Shard = Mutex<FxHashSet<(u64, u64)>>;

impl Seen {
    /// Numbers the bitmap covers: sixty-four million, more files than a
    /// desktop volume holds, for 8 MiB written once.
    const BITS: u64 = 1 << 26;

    /// Hash set shards, as a power of two.
    const SHARD_BITS: u32 = 6;

    /// The record part of an NTFS file id: its reuse count sits above.
    const RECORD: u64 = (1 << 48) - 1;

    /// Keys compared whole.
    pub(crate) fn new() -> Self {
        Self::with_mask(u64::MAX)
    }

    /// Keys of the first volume compared by NTFS record: only for ids
    /// that are all current, where one record is one file.
    pub(crate) fn by_record() -> Self {
        Self::with_mask(Self::RECORD)
    }

    fn with_mask(mask: u64) -> Self {
        Self {
            volume: OnceLock::new(),
            bits: LazyLock::new(|| {
                std::iter::repeat_with(|| AtomicU64::new(0))
                    .take((Self::BITS / 64) as usize)
                    .collect()
            }),
            rest: std::iter::repeat_with(Mutex::default)
                .take(1 << Self::SHARD_BITS)
                .collect(),
            mask,
        }
    }

    /// Whether `key` is new.
    pub(crate) fn insert(&self, key: (u64, u64)) -> bool {
        let (volume, number) = key;
        let bit_at = number & self.mask;
        if *self.volume.get_or_init(|| volume) == volume && bit_at < Self::BITS
        {
            let bit = 1 << (bit_at % 64);
            let word = &self.bits[(bit_at / 64) as usize];
            return word.fetch_or(bit, Ordering::Relaxed) & bit == 0;
        }
        // The top bits of a multiplicative hash, with another constant
        // than the set's own hasher, so a shard's keys still spread over
        // its table.
        let shard = (number.wrapping_mul(0x9E37_79B9_7F4A_7C15)
            >> (u64::BITS - Self::SHARD_BITS)) as usize;
        crate::scan::lock(&self.rest[shard]).insert(key)
    }
}

/// Absolute path of the node at `crumbs` beneath a scanned root.
pub fn path_of(root_path: &Path, tree: &Tree, crumbs: &[usize]) -> PathBuf {
    let mut path = root_path.to_path_buf();
    let mut node = tree.root();
    for &index in crumbs {
        match node.child(index) {
            Some(child) => {
                path.push(child.name());
                node = child;
            }
            None => break,
        }
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(name: &str, bytes: u64) -> Draft {
        Draft::entry(name, NodeKind::File, bytes)
    }

    fn dir(name: &str, children: Vec<Draft>) -> Draft {
        Draft {
            children,
            ..Draft::directory(name)
        }
    }

    #[test]
    fn seen_charges_an_identity_once_in_either_store() {
        let seen = Seen::new();
        // The first volume: the bitmap.
        assert!(seen.insert((7, 42)));
        assert!(!seen.insert((7, 42)));
        assert!(seen.insert((7, 43)));
        // A number past the bitmap, and another volume: the hash set.
        let far = Seen::BITS + 42;
        assert!(seen.insert((7, far)));
        assert!(!seen.insert((7, far)));
        assert!(seen.insert((8, 42)), "another volume's 42 is another file");
        assert!(!seen.insert((8, 42)));
    }

    #[test]
    fn seen_by_record_takes_a_reused_record_for_one_file() {
        let (older, newer) = (0x0001_0000_0000_002A, 0x0002_0000_0000_002A);
        let whole = Seen::new();
        assert!(whole.insert((7, older)));
        assert!(whole.insert((7, newer)), "a kept subtree's older file");
        // Only a walk of current ids keys by record: one record, one file.
        let current = Seen::by_record();
        assert!(current.insert((7, older)));
        assert!(!current.insert((7, newer)));
        assert!(current.insert((7, 0x0002_0000_0000_002B)));
        // Past the first volume, keys stay whole.
        assert!(current.insert((8, older)));
        assert!(current.insert((8, newer)));
    }

    #[test]
    fn totals_are_derived_and_children_ordered() {
        let tree = Tree::from_draft(
            dir(
                "root",
                vec![
                    dir("child", vec![leaf("deep", 7)]),
                    leaf("direct", 5),
                    leaf("small", 9),
                ],
            ),
            Metric::Bytes,
        );
        let root = tree.root();
        assert_eq!(root.own_bytes(), 5 + 9, "direct leaves only");
        assert_eq!(root.bytes(), 5 + 9 + 7);
        assert_eq!(root.files(), 3);
        assert_eq!(root.own_files(), 2);
        assert_eq!(root.dirs(), 2);
        assert_eq!(root.child_named("child").map(Node::own_bytes), Some(7));
        let names: Vec<&str> = root.children().map(Node::name).collect();
        assert_eq!(names, ["small", "child", "direct"], "largest first");
        let solo = root.child(0).expect("small");
        assert_eq!((solo.bytes(), solo.own_bytes()), (9, 9));
        assert_eq!((solo.files(), solo.dirs()), (1, 0));
    }

    #[test]
    fn reorder_ranks_by_file_count_and_back() {
        fn first(tree: &Tree) -> Option<&str> {
            tree.root().child(0).map(Node::name)
        }
        let many = (0..5).map(|index| leaf(&format!("f{index}"), 1)).collect();
        let mut tree = Tree::from_draft(
            dir("root", vec![dir("many", many), leaf("huge", 10_000)]),
            Metric::Bytes,
        );
        assert_eq!(first(&tree), Some("huge"));
        tree.reorder(Metric::Files);
        assert_eq!(first(&tree), Some("many"));
        assert_eq!(tree.root().child(0).map(Node::files), Some(5));
        tree.reorder(Metric::Bytes);
        assert_eq!(first(&tree), Some("huge"));
    }

    #[test]
    fn resolve_walks_child_indices() {
        let tree = Tree::from_draft(
            dir("root", vec![dir("child", vec![leaf("deep", 1)])]),
            Metric::Bytes,
        );
        assert!(tree.resolve(&[]).is_some());
        assert_eq!(tree.resolve(&[0, 0]).map(Node::name), Some("deep"));
        assert!(tree.resolve(&[0, 1]).is_none());
        assert_eq!(tree.root().resolve_chain(&[0, 0]).len(), 3);
        assert_eq!(tree.root().depth(), 2);
    }

    #[test]
    fn path_of_joins_names_beneath_the_root() {
        let tree = Tree::from_draft(
            dir("root", vec![dir("child", vec![leaf("deep", 1)])]),
            Metric::Bytes,
        );
        let path = path_of(Path::new("/home/tobi"), &tree, &[0, 0]);
        assert_eq!(path, PathBuf::from("/home/tobi/child/deep"));
    }

    #[test]
    fn largest_child_is_the_first_child_because_children_are_sorted() {
        let tree = Tree::from_draft(
            dir("root", vec![leaf("small", 1), leaf("big", 10)]),
            Metric::Bytes,
        );
        assert_eq!(tree.root().largest_child(), Some(0));
        assert_eq!(tree.root().child(0).map(Node::name), Some("big"));
        let empty = Tree::from_draft(Draft::directory("empty"), Metric::Bytes);
        assert_eq!(empty.root().largest_child(), None);
    }

    #[test]
    fn a_file_is_of_the_kind_of_what_holds_it() {
        let mut cache = dir("cache", vec![leaf("blob", 3)]);
        cache.category = Category::Cache;
        cache.reclaim = Some(Reclaim::Regenerable);
        let tree = Tree::from_draft(
            dir("root", vec![cache, leaf("Music", 1)]),
            Metric::Bytes,
        );
        let blob = tree.resolve(&[0, 0]).expect("blob");
        assert_eq!(blob.kinds(), (Category::Cache, Some(Reclaim::Regenerable)));
        // Beneath the root, by its name.
        let music = tree.resolve(&[1]).expect("music");
        assert_eq!(music.category(), Category::Media);
    }

    #[test]
    fn a_reserve_past_what_a_tree_numbers_takes_nothing() {
        let builder = Builder::new(1);
        assert_eq!(builder.reserve(NONE), None);
        assert_eq!(builder.reserve(2), Some(0));
        assert_eq!(builder.reserve(NONE - 2), None);
        assert_eq!(builder.reserve(1), Some(2));
    }

    #[test]
    fn threads_building_at_once_make_one_tree() {
        let builder = Builder::new(4);
        let root = builder.reserve(1).expect("root");
        let base = builder.reserve(64).expect("subdirectories");
        (0..64_u32).into_par_iter().for_each(|index| {
            let names: Vec<String> =
                (0..300).map(|file| format!("{index}-{file}")).collect();
            let run = names.iter().map(|name| {
                let item = Item {
                    value: 1,
                    ..Item::default()
                };
                (item, name.as_str())
            });
            let text = names.iter().map(String::len).sum();
            let dir = Dir {
                parent: root,
                bytes: 300,
                files: 300,
                ..Dir::EMPTY
            };
            builder.place(base + index, dir, run, text).expect("placed");
        });
        let items = (0..64_u32).map(|index| {
            let item = Item {
                kind: DIRECTORY,
                value: u64::from(base + index),
                ..Item::default()
            };
            (item, "d")
        });
        builder.place(root, Dir::EMPTY, items, 64).expect("placed");
        let mut tree = builder.finish("root".into());
        tree.settle(root, Metric::Bytes);
        let root = tree.root();
        assert_eq!((root.files(), root.dirs()), (64 * 300, 65));
        // Each directory holds its own files, whichever thread placed them.
        for child in root.children() {
            assert_eq!(child.child_count(), 300);
            let owner = |file: Node<'_>| {
                file.name().split('-').next().map(str::to_owned)
            };
            let first = child.child(0).and_then(owner);
            assert!(child.children().all(|file| owner(file) == first));
        }
    }
}

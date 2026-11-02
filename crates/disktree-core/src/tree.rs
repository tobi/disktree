//! The scanned tree.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use rustc_hash::FxHashSet;

use crate::path_style::PathStyle;

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
}

/// How a node's importance is measured.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
)]
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

pub use crate::index::{
    Block, Builder, Built, Children, Facts, Iter, MAX_NAME_BYTES, Node,
    NodeMut, Sibling, StorageBytes, Tree,
};

/// A node and everything beneath it, owned and on the heap.
///
/// How tests and small trees are written before they become a [`Tree`]; a
/// scan builds its tree in place, through [`Tree::build`]. Its
/// facts read and write through `Deref`, as a node's do.
#[derive(Clone, Debug)]
pub struct Draft {
    pub name: Box<str>,
    pub facts: Facts,
    pub allocation: Option<Box<crate::sharing::Allocation>>,
    pub sharing: Option<Box<crate::sharing::Sharing>>,
    pub children: Vec<Self>,
}

impl std::ops::Deref for Draft {
    type Target = Facts;

    fn deref(&self) -> &Facts {
        &self.facts
    }
}

impl std::ops::DerefMut for Draft {
    fn deref_mut(&mut self) -> &mut Facts {
        &mut self.facts
    }
}

impl Draft {
    /// A directory with no children yet.
    pub fn directory(name: impl Into<Box<str>>) -> Self {
        Self::of(name, Facts::directory())
    }

    /// A leaf entry.
    pub fn entry(
        name: impl Into<Box<str>>,
        kind: NodeKind,
        bytes: u64,
    ) -> Self {
        Self::of(name, Facts::entry(kind, bytes))
    }

    fn of(name: impl Into<Box<str>>, facts: Facts) -> Self {
        Self {
            name: name.into(),
            facts,
            allocation: None,
            sharing: None,
            children: Vec::new(),
        }
    }
}

impl Node<'_> {
    /// This node and everything beneath it as a [`Draft`], to change its
    /// shape and build again.
    pub fn to_draft(self) -> Draft {
        let mut draft = Draft::of(self.name(), *self.facts());
        draft.allocation = self.allocation().map(Box::new);
        draft.sharing = self.stored_sharing().copied().map(Box::new);
        draft.children = self.children().iter().map(Self::to_draft).collect();
        draft
    }

    /// Sparse: ordinary files do not pay for five additional byte counters.
    pub fn sharing(self) -> crate::sharing::Sharing {
        self.stored_sharing()
            .copied()
            .unwrap_or_else(|| crate::sharing::leaf_summary(self))
    }

    pub const fn is_dir(self) -> bool {
        self.facts().kind.is_dir()
    }

    /// Bytes of the leaf entries directly in this directory, or this file's
    /// own size.
    pub fn own_bytes(self) -> u64 {
        if !self.is_dir() {
            return self.bytes;
        }
        self.children()
            .iter()
            .filter(|child| !child.is_dir())
            .fold(0, |sum, child| child.bytes.saturating_add(sum))
    }

    /// Files directly in this directory; `1` for a file.
    pub fn own_files(self) -> u64 {
        if !self.is_dir() {
            return self.files;
        }
        self.children()
            .iter()
            .filter(|child| !child.is_dir())
            .map(|child| child.files)
            .sum()
    }

    /// [`Facts::inode`] as the plain pair a platform reports.
    pub fn identity(self) -> Option<(u64, u64)> {
        self.inode.map(|(device, file)| (device, file.get()))
    }

    /// The value a treemap should weight this node by.
    pub const fn value(self, metric: Metric) -> u64 {
        self.facts().value(metric)
    }

    /// The child with this name, if there is one.
    pub fn child_named(self, name: &str) -> Option<Self> {
        self.children().iter().find(|child| child.name() == name)
    }

    pub fn child(self, index: usize) -> Option<Self> {
        self.children().get(index)
    }

    /// The node at `relative`, a path below this one, with the crumbs that
    /// lead there. Components are compared one at a time, so a name holding
    /// a separator cannot stand for two.
    pub fn descend(self, relative: &Path) -> Option<(Vec<usize>, Self)> {
        let mut node = self;
        let mut crumbs = Vec::new();
        for component in relative.components() {
            let name = component.as_os_str().to_string_lossy();
            let index = node
                .children()
                .iter()
                .position(|child| child.name() == name)?;
            node = node.children().at(index);
            crumbs.push(index);
        }
        Some((crumbs, node))
    }

    /// Follow `crumbs` from this node. Crumbs are child indices, so they stay
    /// valid across re-sorting only for the tree they were produced from.
    pub fn resolve(self, crumbs: &[usize]) -> Option<Self> {
        let mut node = self;
        for &index in crumbs {
            node = node.children().get(index)?;
        }
        Some(node)
    }

    /// The chain of nodes ending at `crumbs`, including this node.
    pub fn resolve_chain(self, crumbs: &[usize]) -> Vec<Self> {
        let mut chain = vec![self];
        let mut node = self;
        for &index in crumbs {
            match node.children().get(index) {
                Some(child) => {
                    chain.push(child);
                    node = child;
                }
                None => break,
            }
        }
        chain
    }

    /// Index of the largest child, used to pick a useful descent target.
    pub fn largest_child(self) -> Option<usize> {
        if self.children().is_empty() {
            None
        } else {
            Some(0)
        }
    }

    /// Depth of the deepest descendant.
    pub fn depth(self) -> u32 {
        self.children()
            .iter()
            .map(Self::depth)
            .max()
            .map_or(0, |deepest| deepest + 1)
    }

    /// Breadth-first search for the first node whose name contains `needle`
    /// (case-insensitive), returning its crumbs and the node.
    pub fn find(self, needle: &str) -> Option<(Vec<usize>, Self)> {
        let needle = needle.to_lowercase();
        if needle.is_empty() {
            return None;
        }
        let mut queue = vec![(Vec::new(), self)];
        while let Some((crumbs, node)) = queue.pop() {
            for (index, child) in node.children().iter().enumerate() {
                if child.name().to_lowercase().contains(&needle) {
                    let found: Vec<usize> = crumbs
                        .iter()
                        .copied()
                        .chain(std::iter::once(index))
                        .collect();
                    return Some((found, child));
                }
                if child.is_dir() && !child.children().is_empty() {
                    let mut next = crumbs.clone();
                    next.push(index);
                    queue.push((next, child));
                }
            }
        }
        None
    }
}

/// Recompute `bytes`, `files` and `dirs` bottom-up, then order children by
/// `metric`, largest first.
///
/// A leaf's `bytes` is its measured size and the only input; every total is
/// derived from the leaves, as are the direct figures [`Node::own_bytes`]
/// and [`Node::own_files`]. Deriving them is what keeps them consistent:
/// hardlink de-duplication rewrites a leaf's weight, and every total above
/// it follows without a second pass.
pub fn aggregate(tree: &mut Tree, metric: Metric) {
    aggregate_tree(tree, metric, None);
    crate::sharing::refresh(tree);
}

/// Every directory's totals from its children, then its children in
/// order, with separate subtrees near the top on separate workers.
fn aggregate_tree(tree: &mut Tree, metric: Metric, seen: Option<&Seen>) {
    tree.par_bottom_up(
        |facts| {
            // Every name of a file has the file's size, so one that weighs
            // nothing need not be remembered to be charged once.
            if facts.bytes > 0
                && let Some(seen) = seen
                && let Some((device, file)) = facts.inode
                && !seen.insert((device, file.get()))
            {
                facts.bytes = 0;
            }
        },
        |facts, mut block| {
            let mut bytes = 0;
            let mut files = 0;
            let mut dirs: u32 = 1;
            let mut modified = 0;
            let mut read_error = facts.read_error;
            for child in block.iter() {
                read_error |= child.read_error;
                modified = modified.max(child.modified);
                // Saturating: a corrupt volume's file table can claim any
                // size.
                bytes = child.bytes.saturating_add(bytes);
                files += child.files;
                dirs += child.dirs;
            }
            facts.read_error = read_error;
            facts.bytes = bytes;
            facts.files = files;
            facts.dirs = dirs;
            facts.modified = modified;
            order(&mut block, metric);
        },
    );
}

/// [`aggregate`], charging a hardlinked file once: a leaf whose identity
/// `seen` already holds weighs nothing. Which of a file's names is charged
/// is whichever a worker reaches first, so two scans of an unchanged tree
/// can split it differently between folders; the totals are the same.
pub(crate) fn aggregate_deduped(tree: &mut Tree, metric: Metric, seen: &Seen) {
    aggregate_tree(tree, metric, Some(seen));
    crate::sharing::refresh(tree);
}

/// Identities a finish pass has met.
///
/// A hash set of every file on a disk is millions of random writes into a
/// table too big for any cache. Inode and file record numbers are small
/// integers, though, so a key on the first volume met whose whole number
/// is below the bitmap's size gets one bit instead: an 8 MiB bitmap the
/// workers set without locking. The file table reader's record numbers
/// fit, as do most inodes. Anything else goes into the hash set, whole:
/// a Windows file id keeps the record's reuse count in its top 16 bits,
/// and that is what tells a file from an older one on the same record,
/// which a subtree kept from an earlier scan can still hold. The set is
/// sharded for the walk's millions of those: 4.1 million ids took 150 ms
/// across 64 locks and 1.2 s behind one, in a synthetic run.
pub(crate) struct Seen {
    /// The first volume met, which is the scanned root's in all but a scan
    /// that leaves its volume.
    volume: OnceLock<u64>,
    /// Made on the first key that fits: most Unix trees, where only files
    /// with more than one name carry an identity, never need it.
    bits: LazyLock<Box<[AtomicU64]>>,
    rest: Box<[Shard]>,
}

/// One lock's part of the hash set.
type Shard = Mutex<FxHashSet<(u64, u64)>>;

impl Seen {
    /// Numbers the bitmap covers: sixty-four million, more files than a
    /// desktop volume holds, for 8 MiB written once.
    const BITS: u64 = 1 << 26;

    /// Hash set shards, as a power of two.
    const SHARD_BITS: u32 = 6;

    pub(crate) fn new() -> Self {
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
        }
    }

    /// Whether `key` is new.
    pub(crate) fn insert(&self, key: (u64, u64)) -> bool {
        let (volume, number) = key;
        if *self.volume.get_or_init(|| volume) == volume && number < Self::BITS
        {
            let bit = 1 << (number % 64);
            let word = &self.bits[(number / 64) as usize];
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

/// Levels whose subtrees are built in parallel. A top level already
/// splits a disk into enough work for every thread; below it a parallel
/// frame per level would only cut the depth that fits in a worker's stack.
pub(crate) const PARALLEL_LEVELS: usize = 4;

/// Order every directory's children by `metric`, largest first, leaving
/// every total and sharing figure as it is.
///
/// For a tree [`aggregate`] has already derived, this is all a change of
/// metric needs: totals and sharing are sums and unions over children, so
/// the order they are visited in changes none of them. On a scanned tree
/// of 19 million nodes this took 0.03 to 0.06 s; the sharing pass that
/// `aggregate` adds took 0.5 to 0.8 s.
pub fn reorder(tree: &mut Tree, metric: Metric) {
    tree.par_bottom_up(|_| {}, |_, mut block| order(&mut block, metric));
}

fn order(block: &mut Block<'_>, metric: Metric) {
    // Largest first: a treemap lays out big tiles best, and the order is what
    // makes "descend into the largest child" meaningful. Unstable: names in
    // one directory are distinct, and the only ties are names that decoded
    // to the same lossy text, whose order does not matter, so the stable
    // sort's scratch allocation buys nothing.
    block.sort_by(|left, right| {
        right
            .value(metric)
            .cmp(&left.value(metric))
            .then_with(|| left.name.cmp(right.name))
    });
}

/// Absolute path of the node at `crumbs` beneath a scanned root, joined by
/// the rules of the machine that was scanned.
pub fn path_of(
    root_path: &Path,
    root: Node<'_>,
    crumbs: &[usize],
    style: PathStyle,
) -> PathBuf {
    let mut path = root_path.to_path_buf();
    let mut node = root;
    for &index in crumbs {
        match node.children().get(index) {
            Some(child) => {
                style.push(&mut path, child.name());
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
    use std::num::NonZeroU64;

    fn leaf(name: &str, bytes: u64) -> Draft {
        Draft::entry(name, NodeKind::File, bytes)
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
    fn identities_that_differ_only_in_their_top_bits_are_two_files() {
        // Record 0x2A of one volume under two reuse counts, as a subtree kept
        // from an earlier scan and a later file can hold, and a real second
        // name of the newer one.
        let older = (7, (1 << 48) | 0x2A);
        let newer = (7, (2 << 48) | 0x2A);
        let mut root = Draft::directory("root");
        for (name, bytes, key) in [
            ("older", 4096, older),
            ("newer", 8192, newer),
            ("newer again", 8192, newer),
        ] {
            let mut file = leaf(name, bytes);
            file.inode = Some((key.0, NonZeroU64::new(key.1).unwrap()));
            root.children.push(file);
        }
        let mut tree = Tree::from(root);
        aggregate_deduped(&mut tree, Metric::Bytes, &Seen::new());
        assert_eq!(tree.root().bytes, 4096 + 8192);
    }

    #[test]
    fn aggregate_derives_totals_and_orders_children() {
        let mut root = Draft::directory("root");
        let mut nested = Draft::directory("child");
        nested.children.push(leaf("deep", 7));
        root.children.push(nested);
        root.children.push(leaf("direct", 5));
        root.children.push(leaf("small", 9));

        let mut tree = Tree::from(root);
        aggregate(&mut tree, Metric::Bytes);
        let root = tree.root();
        assert_eq!(root.own_bytes(), 5 + 9, "direct leaves only");
        assert_eq!(root.bytes, 5 + 9 + 7);
        assert_eq!(root.files, 3);
        assert_eq!(root.own_files(), 2);
        assert_eq!(root.dirs, 2);
        assert_eq!(child_named(root, "child").own_bytes(), 7);
        assert_eq!(
            root.children().at(0).name(),
            "small",
            "9 bytes, largest first"
        );
        assert_eq!(root.children().at(2).name(), "direct");

        // A file's own size is what it weighs; aggregates never invent more.
        let mut single = Tree::from(leaf("solo", 3));
        aggregate(&mut single, Metric::Bytes);
        let single = single.root();
        assert_eq!(single.bytes, 3);
        assert_eq!(single.own_bytes(), 3);
        assert_eq!(single.files, 1);
        assert_eq!(single.dirs, 0);
    }

    fn child_named<'a>(node: Node<'a>, name: &str) -> Node<'a> {
        node.children()
            .iter()
            .find(|child| child.name() == name)
            .unwrap_or_else(|| panic!("no child named {name}"))
    }

    #[test]
    fn aggregate_can_rank_by_file_count() {
        let mut root = Draft::directory("root");
        let mut many = Draft::directory("many");
        for index in 0..5 {
            many.children.push(leaf(&format!("f{index}"), 1));
        }
        root.children.push(many);
        root.children.push(leaf("huge", 10_000));

        let mut tree = Tree::from(root);
        aggregate(&mut tree, Metric::Bytes);
        assert_eq!(tree.root().children().at(0).name(), "huge");
        aggregate(&mut tree, Metric::Files);
        assert_eq!(tree.root().children().at(0).name(), "many");
        assert_eq!(tree.root().children().at(0).files, 5);
    }

    #[test]
    fn reorder_matches_aggregate_without_touching_totals() {
        let mut root = Draft::directory("root");
        let mut many = Draft::directory("many");
        for index in 0..5 {
            many.children.push(leaf(&format!("f{index}"), 1));
        }
        many.children.push(leaf("big", 50));
        root.children.push(many);
        root.children.push(leaf("huge", 10_000));
        let mut root = Tree::from(root);
        aggregate(&mut root, Metric::Bytes);

        for metric in [Metric::Files, Metric::Bytes, Metric::Files] {
            let mut reordered = root.clone();
            reorder(&mut reordered, metric);
            aggregate(&mut root, metric);
            let (reordered, root) = (reordered.root(), root.root());
            assert_eq!(names(reordered), names(root), "{metric:?}");
            assert_eq!(reordered.bytes, root.bytes);
            assert_eq!(reordered.files, root.files);
            assert_eq!(reordered.dirs, root.dirs);
            assert_eq!(reordered.sharing(), root.sharing());
        }
    }

    fn names(node: Node<'_>) -> Vec<String> {
        std::iter::once(node.name().to_string())
            .chain(node.children().iter().flat_map(names))
            .collect()
    }

    #[test]
    fn resolve_walks_child_indices() {
        let mut root = Draft::directory("root");
        let mut nested = Draft::directory("child");
        nested.children.push(leaf("deep", 1));
        root.children.push(nested);
        let tree = Tree::from(root);
        let root = tree.root();

        assert!(root.resolve(&[]).is_some());
        assert_eq!(root.resolve(&[0, 0]).map(Node::name), Some("deep"));
        assert!(root.resolve(&[0, 1]).is_none());
        assert_eq!(root.resolve_chain(&[0, 0]).len(), 3);
    }

    #[test]
    fn path_of_joins_names_beneath_the_root() {
        let mut root = Draft::directory("root");
        let mut nested = Draft::directory("child");
        nested.children.push(leaf("deep", 1));
        root.children.push(nested);
        let tree = Tree::from(root);
        let root = tree.root();

        let path =
            path_of(Path::new("/home/tobi"), root, &[0, 0], PathStyle::Native);
        assert_eq!(path, PathBuf::from("/home/tobi/child/deep"));
        // A Unix host's path is joined with `/` on every client.
        let path =
            path_of(Path::new("/home/tobi"), root, &[0, 0], PathStyle::Posix);
        assert_eq!(path.to_str(), Some("/home/tobi/child/deep"));
    }

    #[test]
    fn largest_child_is_the_first_child_because_children_are_sorted() {
        let mut root = Draft::directory("root");
        root.children.push(leaf("big", 10));
        root.children.push(leaf("small", 1));
        assert_eq!(Tree::from(root).root().largest_child(), Some(0));
        assert_eq!(
            Tree::from(Draft::directory("empty")).root().largest_child(),
            None
        );
    }

    #[test]
    fn find_reports_crumbs_and_is_case_insensitive() {
        let mut root = Draft::directory("root");
        let mut target = Draft::directory("target");
        target.children.push(leaf("needle-file", 1));
        root.children.push(target);
        let tree = Tree::from(root);
        let root = tree.root();

        let (crumbs, found) = root.find("NEEDLE").expect("found");
        assert_eq!(crumbs, vec![0, 0]);
        assert_eq!(found.name(), "needle-file");
        assert!(root.find("").is_none());
        assert!(root.find("absent").is_none());
    }

    #[test]
    fn depth_counts_edges() {
        let mut root = Draft::directory("root");
        let mut nested = Draft::directory("child");
        nested.children.push(leaf("deep", 1));
        root.children.push(nested);
        assert_eq!(Tree::from(root).root().depth(), 2);
        assert_eq!(Tree::from(leaf("x", 0)).root().depth(), 0);
    }
}

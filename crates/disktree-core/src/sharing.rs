//! APFS sharing, reported separately from the treemap's allocated bytes.
//!
//! Harold Hunt's clone accounting, ported from huntharo/disktree#3. He
//! first wrote it for diskhound (huntharo/diskhound#4, contributed as
//! tzarebczan/diskhound#40). Clone IDs describe full data-fork clones, not
//! every shared extent of modified files.

use rayon::prelude::*;
use rustc_hash::FxHashMap;

use std::sync::Mutex;

use crate::tree::{Node, Tree};

mod reclaim;
pub(crate) use reclaim::Selected;
pub use reclaim::{Estimate, estimate};

/// Sparse allocation facts survive hardlink weight de-duplication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Allocation {
    pub bytes: u64,
    pub identity: Option<(u64, u64)>,
    pub links: u64,
    pub followed: bool,
    pub clone: Option<CloneInfo>,
}

/// Metadata retained only for files that may share allocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloneInfo {
    pub device: u64,
    pub id: u64,
    pub references: u32,
    /// Private data-fork allocation; other forks can remain unmeasured.
    pub private_bytes: Option<u64>,
    /// Clone identity describes the data fork, not resource forks or xattrs.
    pub data_bytes: u64,
}

/// Derived figures; none changes `Node::bytes` or a tile's weight.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sharing {
    pub files: u64,
    pub bytes: u64,
    pub private_bytes: u64,
    pub duplicate_bytes: u64,
    pub unknown_files: u64,
    /// More than one million distinct groups: repeated-byte reporting is partial.
    pub groups_truncated: bool,
    pub estimate: Option<Estimate>,
}

impl Sharing {
    /// Lower bound before snapshots or changes since the scan.
    pub fn reclaimable(self, listed: u64) -> u64 {
        self.estimate.map_or(listed, |estimate| estimate.lower)
    }

    pub fn possible(self, listed: u64) -> u64 {
        self.estimate.map_or(listed, |estimate| estimate.upper)
    }

    /// Allocation in clone files not known to be private. Other clones,
    /// selected or not and inside the scan or not, may hold these blocks,
    /// so deleting this alone does not free them. An unknown private size
    /// counts as shared: the conservative side for a saving.
    pub const fn shared_bytes(self) -> u64 {
        self.bytes.saturating_sub(self.private_bytes)
    }
}

/// Leaf metadata is already retained; do not allocate another box per clone.
pub(crate) fn leaf_summary(node: Node<'_>) -> Sharing {
    let Some(allocation) = node.allocation() else {
        return Sharing::default();
    };
    let mut summary = Sharing {
        estimate: Some(allocation.estimate()),
        ..Sharing::default()
    };
    own(node, &mut summary);
    summary
}

impl Allocation {
    fn estimate(self) -> Estimate {
        if self.followed || (self.links > 1 && self.identity.is_some()) {
            return Estimate::default();
        }
        if self.links > 1 {
            return Estimate {
                upper: self.bytes,
                partial: true,
                ..Estimate::default()
            };
        }
        let lower = self.clone.map_or(self.bytes, |info| {
            info.private_bytes.unwrap_or(0).min(self.bytes)
        });
        let full = self.clone.is_some_and(|info| {
            info.device != 0
                && info.id != 0
                && info.references >= 2
                && info.private_bytes == Some(0)
        });
        Estimate {
            lower,
            upper: if full {
                self.bytes.saturating_sub(
                    self.clone.map_or(0, |info| info.data_bytes),
                )
            } else {
                self.bytes
            },
            partial: self.clone.is_some_and(CloneInfo::incomplete),
        }
    }
}

impl CloneInfo {
    const fn incomplete(self) -> bool {
        self.device == 0
            || self.id == 0
            || self.references == 0
            || self.private_bytes.is_none()
    }
}

#[derive(Debug)]
struct Group {
    bytes: u64,
    count: u64,
}

impl Group {
    const fn duplicates(&self) -> u64 {
        self.bytes.saturating_mul(self.count - 1)
    }
}

type Groups = FxHashMap<(u64, u64), Group>;

/// Union duplicate reporting uses exactly the same grouping as aggregation.
#[derive(Default)]
pub(crate) struct DuplicateSummary {
    groups: Groups,
    summary: Sharing,
    incomplete: bool,
}

impl DuplicateSummary {
    pub(crate) fn add(&mut self, node: Node<'_>) {
        self.incomplete |= node
            .allocation()
            .and_then(|allocation| allocation.clone)
            .is_some_and(CloneInfo::incomplete);
        if let Some((key, group)) = own(node, &mut self.summary) {
            join(
                &mut self.groups,
                key,
                group,
                MAX_CLONE_GROUPS,
                &mut self.summary,
            );
        }
    }

    pub(crate) const fn bytes(&self) -> Option<u64> {
        if self.incomplete || self.summary.groups_truncated {
            None
        } else {
            Some(self.summary.duplicate_bytes)
        }
    }
}
// Bound temporary grouping memory on volumes with millions of clone IDs.
const MAX_CLONE_GROUPS: usize = 1_000_000;

/// Small-to-large merging avoids walking every subtree for every ancestor.
/// Hardlink duplicates already have zero weight when this pass runs.
pub(crate) fn refresh(tree: &mut Tree) {
    collect(tree, MAX_CLONE_GROUPS);
}

/// A node's new sharing figures, by its position in the tree.
type Update = (u32, Option<Sharing>);

/// Where workers leave their updates: one list per run of work, so none
/// is copied into another.
type Sink = Mutex<Vec<Vec<Update>>>;

/// The pass reads the tree on every worker and writes nothing; the
/// figures it settles go into the tree afterwards, by position, since a
/// write would take the whole tree from every other worker.
fn collect(tree: &mut Tree, limit: usize) -> (Groups, reclaim::Accumulator) {
    let sink = Sink::default();
    let mut updates = Vec::new();
    let (groups, allocation, _) =
        collect_at(tree.root(), limit, 0, &mut updates, &sink);
    let mut lists = sink
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    lists.push(updates);
    tree.set_sharing(lists.into_iter().flatten());
    (groups, allocation)
}

/// A file's clone facts in a directory's figures: what [`collect`] of the
/// file would return, merged, without the file's own maps. Most files of
/// a large tree are leaves, and a map apiece was most of this pass's time.
fn fold_leaf(
    leaf: Node<'_>,
    limit: usize,
    part: &mut Part,
    updates: &mut Vec<Update>,
) {
    // Only a directory keeps figures; clear a file's only if it has some,
    // so a pass reads its millions of files without writing to them.
    if leaf.stored_sharing().is_some() {
        updates.push((leaf.id(), None));
    }
    part.allocation.add_leaf(leaf);
    if let Some((key, group)) = own(leaf, &mut part.summary) {
        join(&mut part.groups, key, group, limit, &mut part.summary);
    }
}

/// A node's own clone figures, and its group when it can repeat bytes.
fn own(node: Node<'_>, summary: &mut Sharing) -> Option<((u64, u64), Group)> {
    let info = node
        .allocation()
        .and_then(|allocation| allocation.clone)
        .filter(|_| node.bytes > 0)?;
    summary.files += 1;
    summary.bytes = summary.bytes.saturating_add(node.bytes);
    summary.private_bytes = summary
        .private_bytes
        .saturating_add(info.private_bytes.unwrap_or(0).min(node.bytes));
    summary.unknown_files += u64::from(info.private_bytes.is_none());
    (info.device != 0 && info.id != 0 && info.references >= 2).then(|| {
        (
            (info.device, info.id),
            Group {
                // A modified clone repeats only what it still shares; its
                // private blocks are its own. An unknown private size keeps
                // the whole data fork, as before.
                bytes: info.data_bytes.min(
                    node.bytes.saturating_sub(info.private_bytes.unwrap_or(0)),
                ),
                count: 1,
            },
        )
    })
}

fn join(
    groups: &mut Groups,
    key: (u64, u64),
    group: Group,
    limit: usize,
    summary: &mut Sharing,
) {
    reclaim::work();
    if groups.len() >= limit && !groups.contains_key(&key) {
        summary.groups_truncated = true;
        return;
    }
    groups
        .entry(key)
        .and_modify(|old| {
            // A scan is not atomic. Use the smaller observation when a file
            // changes while its clone group is being measured.
            let before = old.duplicates().saturating_add(group.duplicates());
            old.bytes = old.bytes.min(group.bytes);
            old.count += group.count;
            summary.duplicate_bytes = summary
                .duplicate_bytes
                .saturating_sub(before)
                .saturating_add(old.duplicates());
        })
        .or_insert(group);
}

#[cfg(test)]
thread_local! {
    /// Calls of `collect_at`, each of which makes a node's maps.
    static VISITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A directory's figures while its children are added: what [`collect`]
/// returns, with the sums its children contribute.
struct Part {
    groups: Groups,
    allocation: reclaim::Accumulator,
    summary: Sharing,
}

impl Part {
    fn new(limit: usize) -> Self {
        Self {
            groups: Groups::default(),
            allocation: reclaim::Accumulator::new(limit),
            summary: Sharing::default(),
        }
    }

    /// Add one child: a file folded straight into these maps, or a
    /// directory collected and then merged.
    fn add(
        &mut self,
        child: Node<'_>,
        limit: usize,
        depth: usize,
        updates: &mut Vec<Update>,
        sink: &Sink,
    ) {
        if !child.is_dir() {
            fold_leaf(child, limit, self, updates);
            return;
        }
        let (groups, allocation, summary) =
            collect_at(child, limit, depth + 1, updates, sink);
        self.merge(
            Self {
                groups,
                allocation,
                summary,
            },
            limit,
        );
    }

    /// Merge the figures of children that follow these.
    fn merge(&mut self, mut other: Self, limit: usize) {
        self.allocation.merge(other.allocation);
        let (mine, theirs) = (&mut self.summary, other.summary);
        mine.duplicate_bytes =
            mine.duplicate_bytes.saturating_add(theirs.duplicate_bytes);
        mine.groups_truncated |= theirs.groups_truncated;
        mine.files += theirs.files;
        mine.bytes = mine.bytes.saturating_add(theirs.bytes);
        mine.private_bytes =
            mine.private_bytes.saturating_add(theirs.private_bytes);
        mine.unknown_files += theirs.unknown_files;
        if self.groups.len() < other.groups.len() {
            std::mem::swap(&mut self.groups, &mut other.groups);
        }
        for (key, group) in other.groups {
            join(&mut self.groups, key, group, limit, &mut self.summary);
        }
    }
}

/// `node`'s clone groups and reclaim figures, and the sharing figures it
/// reads as once its update is written, which goes into `updates`.
fn collect_at(
    node: Node<'_>,
    limit: usize,
    depth: usize,
    updates: &mut Vec<Update>,
    sink: &Sink,
) -> (Groups, reclaim::Accumulator, Sharing) {
    #[cfg(test)]
    VISITS.set(VISITS.get() + 1);
    let mut part = Part::new(limit);
    if !node.is_dir() {
        fold_leaf(node, limit, &mut part, updates);
        return (part.groups, part.allocation, leaf_summary(node));
    }
    if let Some((key, group)) = own(node, &mut part.summary) {
        part.groups.insert(key, group);
    }
    if depth < crate::tree::PARALLEL_LEVELS {
        // Subdirectories near the top are independent work, as in the
        // totals pass. Each is merged into this directory's maps as soon
        // as it is done, so the maps held at once are these and those of
        // the subdirectories in progress. Folding runs of children into a
        // set of maps per worker held a set per worker of nearly every
        // group in the tree when the same clones fill every subdirectory:
        // 18 sets of 129 thousand groups, 0.4 GiB at the end of a scan of
        // 8.9 million files of agent worktrees. Merging is commutative,
        // so the figures do not depend on which finishes first, short of
        // the group limit, where which groups are kept already depended
        // on how the work was split.
        for child in node.children() {
            if !child.is_dir() {
                fold_leaf(child, limit, &mut part, updates);
            }
        }
        let merged = Mutex::new(part);
        node.children()
            .par_iter()
            .filter(|child| child.is_dir())
            .for_each(|dir| {
                let mut updates = Vec::new();
                let (groups, allocation, summary) =
                    collect_at(dir, limit, depth + 1, &mut updates, sink);
                if !updates.is_empty() {
                    crate::scan::lock(sink).push(updates);
                }
                crate::scan::lock(&merged).merge(
                    Part {
                        groups,
                        allocation,
                        summary,
                    },
                    limit,
                );
            });
        part = merged
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    } else {
        for child in node.children() {
            part.add(child, limit, depth, updates, sink);
        }
    }
    let Part {
        groups,
        mut allocation,
        mut summary,
    } = part;
    if node.allocation().is_some_and(|info| info.followed) {
        allocation = reclaim::Accumulator::new(limit);
    }
    summary.estimate = Some(allocation.estimate());
    // `set_sharing` rewrites a directory's figures in place, so a second
    // pass allocates nothing for a directory that already shares.
    let kept = summary.files > 0
        || node.allocation().is_some()
        || allocation.needs_selection()
        || summary.reclaimable(node.bytes) != node.bytes
        || summary.possible(node.bytes) != node.bytes;
    // Most directories keep nothing and had nothing: no update to write,
    // as `fold_leaf` does for files.
    if kept || node.stored_sharing().is_some() {
        updates.push((node.id(), kept.then_some(summary)));
    }
    // What `Node::sharing` reads once the update is written: a directory
    // that keeps nothing has no allocation of its own to summarize.
    (
        groups,
        allocation,
        if kept { summary } else { Sharing::default() },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Draft, Tree};
    use crate::tree::{Metric, NodeKind, Seen, aggregate, aggregate_deduped};

    fn clone(name: &str, device: u64, id: u64, private: Option<u64>) -> Draft {
        let mut node = Draft::entry(name, NodeKind::File, 100);
        node.allocation = Some(Box::new(Allocation {
            bytes: node.bytes,
            identity: None,
            links: 1,
            followed: false,
            clone: Some(CloneInfo {
                device,
                id,
                references: 2,
                private_bytes: private,
                data_bytes: 80,
            }),
        }));
        node
    }

    #[test]
    fn selecting_a_complete_clone_group_reclaims_its_data_once() {
        let mut root = Draft::directory("root");
        root.children =
            vec![clone("a", 1, 1, Some(0)), clone("b", 1, 1, Some(0))];
        let mut tree = Tree::from(root);
        aggregate(&mut tree, Metric::Bytes);
        let root = tree.root();
        assert_eq!(root.sharing().reclaimable(root.bytes), 80);
    }

    #[test]
    fn group_limit_marks_the_summary_partial_without_losing_private_bytes() {
        let mut root = Draft::directory("root");
        root.children =
            vec![clone("a", 1, 1, Some(20)), clone("b", 1, 2, Some(20))];
        let mut tree = Tree::from(root);
        aggregate(&mut tree, Metric::Bytes);
        collect(&mut tree, 1);
        let root = tree.root();
        assert!(root.sharing().groups_truncated);
        assert_eq!(root.sharing().private_bytes, 40);
    }

    #[test]
    fn groups_are_volume_scoped_and_only_data_forks_are_duplicated() {
        let mut root = Draft::directory("root");
        root.children = vec![
            clone("a", 1, 42, Some(20)),
            clone("b", 1, 42, Some(20)),
            clone("c", 2, 42, Some(20)),
        ];
        let mut tree = Tree::from(root);
        aggregate(&mut tree, Metric::Bytes);
        let root = tree.root();
        assert_eq!(root.bytes, 300);
        assert_eq!(root.sharing().duplicate_bytes, 80);
        assert_eq!(root.sharing().reclaimable(root.bytes), 140);
        aggregate(&mut tree, Metric::Files);
        assert_eq!(tree.root().sharing().duplicate_bytes, 80);
    }

    #[test]
    fn modified_clones_repeat_only_what_they_still_share() {
        let mut root = Draft::directory("root");
        root.children =
            vec![clone("a", 1, 1, Some(60)), clone("b", 1, 1, Some(60))];
        let mut tree = Tree::from(root);
        aggregate(&mut tree, Metric::Bytes);
        let root = tree.root();
        // Each still shares 40 of its 100 bytes: one copy of that repeats.
        assert_eq!(root.sharing().duplicate_bytes, 40);
        assert!(
            root.sharing().duplicate_bytes <= root.sharing().shared_bytes()
        );
    }

    #[test]
    fn unknown_and_oversized_private_values_stay_conservative() {
        let mut root = Draft::directory("root");
        root.children = vec![
            clone("a", 1, 1, None),
            clone("b", 1, 2, Some(200)),
            Draft::entry("ordinary", NodeKind::File, 30),
        ];
        let mut tree = Tree::from(root);
        aggregate(&mut tree, Metric::Bytes);
        let root = tree.root();
        assert_eq!(root.sharing().unknown_files, 1);
        assert_eq!(root.sharing().reclaimable(root.bytes), 130);
    }

    #[test]
    fn deduplicated_hardlinks_do_not_inflate_sharing() {
        let mut root = Draft::directory("root");
        root.children =
            vec![clone("a", 1, 1, Some(0)), clone("b", 1, 1, Some(0))];
        for node in &mut root.children {
            node.inode =
                Some((1, std::num::NonZeroU64::new(123).expect("inode")));
        }
        let mut tree = Tree::from(root);
        aggregate_deduped(&mut tree, Metric::Bytes, &Seen::new());
        let root = tree.root();
        assert_eq!(root.sharing().files, 1);
        assert_eq!(root.sharing().bytes, 100);
        assert_eq!(root.sharing().duplicate_bytes, 0);
    }

    /// `copies` worktrees nested six levels deep, below the parallel ones,
    /// each holding a clone of the same `files` files, as a directory of
    /// agent worktrees does.
    fn worktrees(copies: u64, files: u64) -> Tree {
        let mut root = Draft::directory("worktrees");
        for copy in 0..copies {
            let mut tree = Draft::directory(format!("w{copy}"));
            for file in 0..files {
                let mut node = clone(&format!("f{file}"), 1, file + 1, Some(0));
                if let Some(info) = node
                    .allocation
                    .as_mut()
                    .and_then(|allocation| allocation.clone.as_mut())
                {
                    info.references = u32::try_from(copies).expect("copies");
                }
                tree.children.push(node);
            }
            for level in 0..6 {
                let mut dir = Draft::directory(format!("w{copy}-{level}"));
                dir.children.push(tree);
                tree = dir;
            }
            root.children.push(tree);
        }
        let mut root = Tree::from(root);
        aggregate(&mut root, Metric::Bytes);
        root
    }

    /// On one thread, so the thread-local counters see all of the pass.
    fn refresh_work(tree: &mut Tree) -> (u64, u64) {
        rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .expect("pool")
            .install(|| {
                VISITS.set(0);
                reclaim::WORK.set(0);
                refresh(tree);
                (VISITS.get(), reclaim::WORK.get())
            })
    }

    /// The pass once took 11 s on 20 million nodes, most of it making,
    /// merging and freeing two maps for every file. Hold it to maps per
    /// directory and to work that grows with the files, not faster.
    #[test]
    fn refresh_work_grows_with_files_and_makes_no_map_per_file() {
        let mut counts = Vec::new();
        for (copies, files) in [(8, 64), (64, 512)] {
            let mut tree = worktrees(copies, files);
            let duplicates = tree.root().sharing().duplicate_bytes;
            assert_eq!(duplicates, 80 * (copies - 1) * files);
            let (visits, work) = refresh_work(&mut tree);
            assert_eq!(tree.root().sharing().duplicate_bytes, duplicates);
            assert_eq!(
                visits,
                u64::from(tree.root().dirs),
                "maps are made per directory, never per file"
            );
            // Each file is added to its directory's two maps, and its
            // group moves into the root's two maps once per other copy:
            // fewer than four steps a file however many there are.
            let budget = 4 * copies * files;
            assert!(work <= budget, "{copies}x{files}: {work} > {budget}");
            counts.push((work, copies * files));
        }
        eprintln!("refresh map steps (work, files): {counts:?}");
    }

    #[test]
    fn deep_and_parallel_levels_merge_like_one_directory() {
        // Members below the parallel levels, beside them and at the top:
        // every path through `collect_at` must reach the same totals.
        let mut flat = Draft::directory("root");
        let mut nested = Draft::directory("root");
        for (depth, private) in [(0, Some(0)), (3, Some(0)), (7, Some(20))] {
            flat.children
                .push(clone(&format!("f{depth}"), 1, 9, private));
            let mut leaf = clone("file", 1, 9, private);
            for level in 0..depth {
                let mut dir = Draft::directory(format!("d{depth}-{level}"));
                dir.children.push(leaf);
                leaf = dir;
            }
            nested.children.push(leaf);
        }
        flat.children.push(Draft::entry("plain", NodeKind::File, 7));
        nested
            .children
            .push(Draft::entry("plain", NodeKind::File, 7));
        let mut flat = Tree::from(flat);
        let mut nested = Tree::from(nested);
        aggregate(&mut flat, Metric::Bytes);
        aggregate(&mut nested, Metric::Bytes);
        assert_eq!(nested.root().sharing(), flat.root().sharing());
        assert_eq!(nested.root().sharing().duplicate_bytes, 2 * 80);

        // A second pass keeps every figure and grows no side table.
        let before = nested.root().sharing();
        let storage = nested.storage_bytes();
        aggregate(&mut nested, Metric::Files);
        assert_eq!(nested.root().sharing(), before);
        assert_eq!(nested.storage_bytes(), storage);

        // A directory that no longer shares gives its figures up.
        let mut draft = nested.root().to_draft();
        let deep = draft
            .children
            .iter_mut()
            .find(|child| child.kind.is_dir())
            .expect("a nested branch");
        let emptied = deep.name.clone();
        deep.children.clear();
        let mut nested = Tree::from(draft);
        aggregate(&mut nested, Metric::Bytes);
        let emptied = nested
            .root()
            .children()
            .iter()
            .find(|child| child.name() == &*emptied)
            .expect("the emptied branch");
        assert!(emptied.stored_sharing().is_none());
        assert_eq!(nested.root().sharing().files, 2);
    }

    /// A tree with a file for every table an allocation can be kept in:
    /// clones with one name, wholly shared, modified and unknown, groups
    /// that span directories and leave it, hardlinks, a followed link and
    /// a clone with a resource fork.
    fn every_kind_of_file() -> Draft {
        let mut root = Draft::directory("root");
        // A clone whose data fork is all of it, as nearly every one is.
        let sized = |name: &str, id, private, references| {
            let mut node = clone(name, 1, id, private);
            node.bytes = 80;
            let allocation = node.allocation.as_mut().expect("allocation");
            allocation.bytes = 80;
            let info = allocation.clone.as_mut().expect("clone");
            info.references = references;
            node
        };
        for (dir, inode) in [("a", 11), ("b", 12)] {
            let mut node = Draft::directory(dir);
            node.children = vec![
                // A third member outside the tree: never complete.
                sized("whole", 7, Some(0), 3),
                sized("complete", 8, Some(0), 2),
                sized("modified", 9, Some(30), 2),
                sized("unknown", 10, None, 2),
                clone("fork", 1, 11, Some(0)),
                Draft::entry("plain", NodeKind::File, 5),
            ];
            let mut link = Draft::entry("link", NodeKind::File, 64);
            link.inode =
                Some((1, std::num::NonZeroU64::new(inode / 10).expect("ino")));
            link.allocation = Some(Box::new(Allocation {
                bytes: 64,
                identity: Some((1, inode / 10)),
                links: 2,
                followed: false,
                clone: None,
            }));
            node.children.push(link);
            let mut followed = sized("followed", 7, Some(0), 3);
            let allocation = followed.allocation.as_mut().expect("allocation");
            allocation.followed = true;
            allocation.clone = None;
            node.children.push(followed);
            root.children.push(node);
        }
        root
    }

    /// Each node's figures, the estimate of each node alone and of every
    /// pair of nodes neither of which holds the other: what a selection
    /// of marks reads.
    fn figures(tree: &Tree) -> Vec<(Sharing, Estimate)> {
        /// Nodes in preorder, each with the end of its subtree's run.
        fn preorder<'t>(node: Node<'t>, nodes: &mut Vec<(Node<'t>, usize)>) {
            let at = nodes.len();
            nodes.push((node, 0));
            for child in node.children() {
                preorder(child, nodes);
            }
            nodes[at].1 = nodes.len();
        }
        let mut nodes = Vec::new();
        preorder(tree.root(), &mut nodes);
        let mut figures = Vec::new();
        for &(node, end) in &nodes {
            figures.push((node.sharing(), estimate([node])));
            // What follows a node's subtree in preorder is outside it, and
            // cannot hold it.
            for &(other, _) in &nodes[end..] {
                figures.push((Sharing::default(), estimate([node, other])));
            }
        }
        figures
    }

    #[test]
    fn every_figure_is_the_same_however_its_allocation_is_kept() {
        let draft = every_kind_of_file();
        let measured = |tree: &mut Tree| {
            aggregate_deduped(tree, Metric::Bytes, &Seen::new());
            let bytes = figures(tree);
            aggregate(tree, Metric::Files);
            (bytes, figures(tree))
        };
        let mut packed = Tree::from(&draft);
        let mut whole = crate::index::kept_whole(|| Tree::from(&draft));
        // The small records are what the packed tree keeps, or this
        // compares the whole tables with themselves.
        assert!(packed.storage_bytes().used < whole.storage_bytes().used);
        let expected = crate::index::kept_whole(|| measured(&mut whole));
        assert_eq!(measured(&mut packed), expected);
        let root = packed.root();
        assert!(root.sharing().duplicate_bytes > 0);
        assert!(root.sharing().unknown_files > 0);
        assert_ne!(root.sharing().reclaimable(root.bytes), root.bytes);
    }

    #[test]
    fn clone_groups_merge_across_directories_and_refresh_after_removal() {
        let mut root = Draft::directory("root");
        for name in ["a", "b"] {
            let mut dir = Draft::directory(name);
            dir.children.push(clone("file", 1, 1, Some(0)));
            root.children.push(dir);
        }
        let mut tree = Tree::from(root);
        aggregate(&mut tree, Metric::Bytes);
        let root = tree.root();
        assert_eq!(root.sharing().duplicate_bytes, 80);
        assert_eq!(root.children().at(0).sharing().duplicate_bytes, 0);
        // Removing a directory is a rescan into a new tree.
        let mut draft = root.to_draft();
        draft.children.pop();
        let mut tree = Tree::from(draft);
        aggregate(&mut tree, Metric::Bytes);
        let root = tree.root();
        assert_eq!(root.sharing().duplicate_bytes, 0);
        assert_eq!(root.sharing().files, 1);
    }

    /// A random tree deeper than the parallel levels: plain files, clones
    /// of a few groups spread across directories (wholly shared, modified,
    /// unknown, on another volume or none known, with a resource fork),
    /// hardlinks across directories and followed links. Every name of a
    /// hardlinked file reads as that one file does, and a file has no
    /// more names than its link count, as a scan of a tree that holds
    /// still finds. With `consistent_links` off, each name has independent
    /// facts and names can exceed the link count, as in a changing tree.
    fn random_tree(
        rng: &mut crate::test_rng::Rng,
        consistent_links: bool,
    ) -> Draft {
        /// Each hardlinked file met so far, and the names it has left.
        type Linked = [Option<(Draft, u64)>; 4];
        fn file(
            rng: &mut crate::test_rng::Rng,
            linked: &mut Linked,
            name: String,
            consistent_links: bool,
        ) -> Draft {
            let bytes = 80 * (rng.below(3) + 1);
            let mut node = Draft::entry(name, NodeKind::File, bytes);
            let kind = rng.below(10);
            if kind < 3 {
                return node;
            }
            let device = [1, 1, 1, 2, 0][rng.below(5) as usize];
            let info = CloneInfo {
                device,
                id: rng.below(5) + 1,
                references: u32::try_from(rng.below(4) + 1).expect("small"),
                private_bytes: [Some(0), Some(0), Some(40), None]
                    [rng.below(4) as usize],
                // A resource fork the group does not share, now and then.
                data_bytes: if rng.percent(15) { bytes - 40 } else { bytes },
            };
            let mut allocation = Allocation {
                bytes,
                identity: None,
                links: 1,
                followed: false,
                clone: Some(info),
            };
            if kind == 8 {
                let inode = rng.below(4) + 1;
                let slot =
                    &mut linked[usize::try_from(inode - 1).expect("ino")];
                match slot {
                    Some((first, left)) if consistent_links && *left > 0 => {
                        *left -= 1;
                        let mut name_of = first.clone();
                        name_of.name = node.name;
                        return name_of;
                    }
                    // Every name met: another file, of one name.
                    Some(_) if consistent_links => {}
                    _ => {
                        node.inode = Some((
                            1,
                            std::num::NonZeroU64::new(inode).expect("ino"),
                        ));
                        allocation.identity = Some((1, inode));
                        allocation.links = rng.below(2) + 2;
                        if rng.percent(50) {
                            allocation.clone = None;
                        }
                        node.allocation = Some(Box::new(allocation));
                        *slot = Some((node.clone(), allocation.links - 1));
                        return node;
                    }
                }
            } else if kind == 9 {
                allocation.followed = true;
            }
            node.allocation = Some(Box::new(allocation));
            node
        }
        fn directory(
            rng: &mut crate::test_rng::Rng,
            linked: &mut Linked,
            name: String,
            depth: usize,
            consistent_links: bool,
        ) -> Draft {
            let mut dir = Draft::directory(name);
            for index in 0..rng.below(4) {
                dir.children.push(file(
                    rng,
                    linked,
                    format!("f{index}"),
                    consistent_links,
                ));
            }
            if depth < 6 {
                for index in 0..rng.below(if depth < 2 { 4 } else { 3 }) {
                    dir.children.push(directory(
                        rng,
                        linked,
                        format!("d{index}"),
                        depth + 1,
                        consistent_links,
                    ));
                }
            }
            // Files and directories in any order, as a listing gives them.
            rng.shuffle(&mut dir.children);
            dir
        }
        directory(
            rng,
            &mut Linked::default(),
            "root".into(),
            0,
            consistent_links,
        )
    }

    /// A pool of `threads` workers, built once per test and shared by its
    /// cases.
    fn pool(threads: usize) -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .expect("a pool")
    }

    /// Every node's figures after a pass on `pool`, ranked by size and
    /// then by files.
    fn on_workers(
        draft: &Draft,
        pool: &rayon::ThreadPool,
        whole: bool,
    ) -> Vec<Vec<(Sharing, Estimate)>> {
        pool.install(|| {
            let build = || Tree::from(draft);
            let mut tree = if whole {
                crate::index::kept_whole(build)
            } else {
                build()
            };
            let mut measured = |metric| {
                if whole {
                    crate::index::kept_whole(|| aggregate(&mut tree, metric));
                } else {
                    aggregate(&mut tree, metric);
                }
                figures(&tree)
            };
            vec![measured(Metric::Bytes), measured(Metric::Files)]
        })
    }

    /// Merging a subdirectory into its parent as it finishes leaves every
    /// figure as merging in child order on one worker does.
    #[test]
    fn random_trees_read_the_same_on_one_worker_and_on_many() {
        let one = pool(1);
        let many: Vec<_> = [2, 4, 8]
            .map(|threads| (threads, pool(threads)))
            .into_iter()
            .collect();
        for seed in 0..300 {
            let draft = random_tree(&mut crate::test_rng::Rng::new(seed), true);
            let expected = on_workers(&draft, &one, false);
            for (threads, pool) in &many {
                assert_eq!(
                    on_workers(&draft, pool, false),
                    expected,
                    "seed {seed} on {threads} workers"
                );
            }
        }
    }

    fn files(node: Node<'_>, into: &mut Vec<Draft>) {
        for child in node.children() {
            if child.is_dir() {
                files(child, into);
            } else {
                into.push(child.to_draft());
            }
        }
    }

    fn check_flat(node: Node<'_>, seed: u64) {
        if !node.is_dir() {
            return;
        }
        let mut flat = Draft::directory("flat");
        files(node, &mut flat.children);
        let mut flat = Tree::from(flat);
        aggregate(&mut flat, Metric::Bytes);
        assert_eq!(
            node.sharing(),
            flat.root().sharing(),
            "seed {seed}: {}",
            node.name()
        );
        assert_eq!(
            estimate([node]),
            estimate([flat.root()]),
            "seed {seed}: selected {}",
            node.name()
        );
        for child in node.children() {
            check_flat(child, seed);
        }
    }

    /// Every directory reads as one directory holding all of its files
    /// would: an answer that does not go through the merging of
    /// subdirectories, near the top or below it, at all.
    #[test]
    fn random_trees_read_as_their_files_would_side_by_side() {
        for seed in 0..300 {
            let draft = random_tree(&mut crate::test_rng::Rng::new(seed), true);
            let mut tree = Tree::from(&draft);
            aggregate(&mut tree, Metric::Bytes);
            check_flat(tree.root(), seed);
        }
    }

    /// Live hardlink observations can disagree or outnumber the reported
    /// links. Neither nesting nor a worker's finishing order changes the
    /// evidence retained in the estimate.
    #[test]
    fn changing_hardlinks_read_the_same_flat_nested_and_on_any_workers() {
        let workers = [1, 2, 4, 8].map(|threads| (threads, pool(threads)));
        for seed in 0..300 {
            let draft =
                random_tree(&mut crate::test_rng::Rng::new(seed), false);
            let mut expected = None;
            for (threads, pool) in &workers {
                let actual = pool.install(|| {
                    let mut tree = Tree::from(&draft);
                    [Metric::Bytes, Metric::Files].map(|metric| {
                        aggregate(&mut tree, metric);
                        check_flat(tree.root(), seed);
                        figures(&tree)
                    })
                });
                if let Some(expected) = &expected {
                    assert_eq!(
                        &actual, expected,
                        "seed {seed} on {threads} workers"
                    );
                } else {
                    expected = Some(actual);
                }
            }
        }
    }

    /// Files of a group sharing a record read as each keeping its own.
    #[test]
    fn random_trees_read_the_same_however_their_allocations_are_kept() {
        let workers = pool(4);
        let mut reused = 0;
        for seed in 0..300 {
            let draft = random_tree(&mut crate::test_rng::Rng::new(seed), true);
            assert_eq!(
                on_workers(&draft, &workers, false),
                on_workers(&draft, &workers, true),
                "seed {seed}"
            );
            let tree = Tree::from(&draft);
            let (records, _, _) = tree.side_records();
            let kept = allocations(tree.root());
            reused += kept - records;
        }
        // Or this compares whole records with themselves.
        assert!(reused > 100, "{reused}");
    }

    fn allocations(node: Node<'_>) -> usize {
        usize::from(node.allocation().is_some())
            + node.children().iter().map(allocations).sum::<usize>()
    }
}

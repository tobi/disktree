//! Composable selected-set accounting. No pairwise mark or group comparisons.

use rustc_hash::FxHashMap;

use super::{Allocation, CloneInfo};
use crate::tree::Node;

/// Scan-derived bounds before snapshots, open files, compression and changes
/// after the scan. Only the lower bound is projected onto the volume meter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Estimate {
    pub lower: u64,
    pub upper: u64,
    pub partial: bool,
}

#[derive(Clone, Copy, Default)]
struct Group {
    members: u64,
    references: u32,
    shared: u64,
    all_zero_private: bool,
}

impl Group {
    /// One file's membership of its clone group.
    fn member(info: CloneInfo, allocated: u64) -> Self {
        Self {
            members: 1,
            references: info.references,
            shared: info.data_bytes.min(
                allocated
                    .saturating_sub(info.private_bytes.unwrap_or(allocated)),
            ),
            all_zero_private: info.private_bytes == Some(0),
        }
    }

    fn credit(&self) -> Estimate {
        let complete =
            self.references >= 2 && self.members == u64::from(self.references);
        Estimate {
            lower: if complete { self.shared } else { 0 },
            upper: if complete && self.all_zero_private {
                self.shared
            } else {
                0
            },
            partial: false,
        }
    }
}

struct Links {
    allocation: Allocation,
    names: u64,
}

impl Links {
    const fn complete(&self) -> bool {
        self.names == self.allocation.links
    }
}

#[derive(Default)]
pub(super) struct Accumulator {
    base: Estimate,
    credit: Estimate,
    groups: FxHashMap<(u64, u64), Group>,
    links: FxHashMap<(u64, u64), Links>,
    limit: usize,
    incomplete: bool,
}

impl Accumulator {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    pub(super) fn needs_selection(&self) -> bool {
        !self.groups.is_empty() || !self.links.is_empty() || self.base.partial
    }

    pub(super) const fn estimate(&self) -> Estimate {
        Estimate {
            lower: self.base.lower.saturating_add(if self.base.partial {
                0
            } else {
                self.credit.lower
            }),
            upper: self.base.upper.saturating_add(self.credit.upper),
            partial: self.base.partial || self.incomplete,
        }
    }

    pub(super) fn leaf(node: Node<'_>, limit: usize) -> Self {
        work();
        let mut acc = Self::new(limit);
        acc.add_leaf(node);
        acc
    }

    /// Add one file: its private and possible bytes, its clone group, or
    /// its name among a hardlink's. This is what merging a file's own
    /// accumulator did, without making one: a tree of millions of clones
    /// allocated, merged and freed a set of maps for every file.
    pub(super) fn add_leaf(&mut self, node: Node<'_>) {
        let Some(allocation) = node.allocation() else {
            self.base.lower = self.base.lower.saturating_add(node.bytes);
            self.base.upper = self.base.upper.saturating_add(node.bytes);
            return;
        };
        if allocation.followed {
            return;
        }
        if allocation.links > 1 {
            if let Some(key) = allocation.identity {
                self.join_links(
                    key,
                    Links {
                        allocation,
                        names: 1,
                    },
                );
            } else {
                self.base.partial = true;
                self.base.upper =
                    self.base.upper.saturating_add(allocation.bytes);
            }
            return;
        }
        if let Some(info) = self.bounds(&allocation, true) {
            work();
            self.join(
                (info.device, info.id),
                Group::member(info, allocation.bytes),
            );
        }
    }

    fn file(&mut self, allocation: &Allocation, add: bool) {
        if let Some(info) = self.bounds(allocation, add) {
            self.member(info, allocation.bytes, add);
        }
    }

    /// A file's private and possible bytes; its clone facts when they can
    /// join a group.
    fn bounds(
        &mut self,
        allocation: &Allocation,
        add: bool,
    ) -> Option<CloneInfo> {
        let info = allocation.clone;
        // Missing clone facts may widen the bounds but cannot prove group
        // completeness. Keep that limitation when crossing the window link.
        self.incomplete |= info.is_some_and(CloneInfo::incomplete);
        let private = info.map_or(allocation.bytes, |info| {
            info.private_bytes.unwrap_or(0).min(allocation.bytes)
        });
        let grouped = info.filter(|info| {
            info.device != 0
                && info.id != 0
                && info.references >= 2
                && info.private_bytes.is_some()
        });
        // A confirmed full clone retains no blocks while an external member
        // remains. Modified/unknown sharing only supplies a lower bound.
        let upper = if let Some(info) = grouped.filter(|_| private == 0) {
            // PRIVATESIZE describes the data fork. Other forks may still
            // have allocation even when that fork is fully shared.
            allocation.bytes.saturating_sub(info.data_bytes)
        } else {
            allocation.bytes
        };
        adjust(&mut self.base.lower, private, add);
        adjust(&mut self.base.upper, upper, add);
        grouped
    }

    fn member(&mut self, info: CloneInfo, allocated: u64, add: bool) {
        work();
        let key = (info.device, info.id);
        if !self.groups.contains_key(&key) && self.groups.len() >= self.limit {
            self.base.partial = true;
            // Group credit cannot be proved, but these blocks may be freed.
            if add && info.private_bytes == Some(0) {
                self.base.upper = self.base.upper.saturating_add(allocated);
            }
            return;
        }
        let group = self.groups.entry(key).or_insert_with(|| Group {
            all_zero_private: true,
            shared: u64::MAX,
            ..Group::default()
        });
        let before = group.credit();
        let file = Group::member(info, allocated);
        adjust(&mut group.members, file.members, add);
        group.references = group.references.max(file.references);
        group.shared = group.shared.min(file.shared);
        group.all_zero_private &= file.all_zero_private;
        replace(&mut self.credit, before, group.credit());
    }

    pub(super) fn merge(&mut self, mut other: Self) {
        // Every entry moves only into a map at least as large. This also
        // bounds long chains and groups distributed across many directories.
        if self.groups.len() + self.links.len()
            < other.groups.len() + other.links.len()
        {
            std::mem::swap(self, &mut other);
        }
        // Re-evaluate completed hardlinks after names from both sets merge.
        // Removing their old contribution first avoids counting a clone
        // member twice when its names straddle selected directories.
        let other_links = std::mem::take(&mut other.links);
        for links in other_links.values().filter(|links| links.complete()) {
            other.file(&links.allocation, false);
        }
        self.base.lower = self.base.lower.saturating_add(other.base.lower);
        self.base.upper = self.base.upper.saturating_add(other.base.upper);
        self.base.partial |= other.base.partial;
        self.incomplete |= other.incomplete;
        for (key, group) in other.groups {
            work();
            self.join(key, group);
        }
        for (key, links) in other_links {
            work();
            self.join_links(key, links);
        }
    }

    fn join(&mut self, key: (u64, u64), group: Group) {
        if !self.groups.contains_key(&key) && self.groups.len() >= self.limit {
            self.base.partial = true;
            self.base.upper = self.base.upper.saturating_add(group.shared);
            return;
        }
        let old = self.groups.entry(key).or_insert_with(|| Group {
            shared: u64::MAX,
            all_zero_private: true,
            ..Group::default()
        });
        let before = old.credit();
        old.members += group.members;
        old.references = old.references.max(group.references);
        old.shared = old.shared.min(group.shared);
        old.all_zero_private &= group.all_zero_private;
        replace(&mut self.credit, before, old.credit());
    }

    fn join_links(&mut self, key: (u64, u64), links: Links) {
        if let Some(old) = self.links.remove(&key) {
            if old.complete() {
                self.file(&old.allocation, false);
            }
            let mut allocation = old.allocation;
            allocation.bytes = allocation.bytes.min(links.allocation.bytes);
            allocation.links = allocation.links.max(links.allocation.links);
            if allocation.clone != links.allocation.clone {
                // Inconsistent live observations cannot authorize saving.
                allocation.clone = Some(CloneInfo::default());
            }
            let joined = Links {
                allocation,
                names: old.names + links.names,
            };
            if joined.complete() {
                self.file(&joined.allocation, true);
            }
            self.links.insert(key, joined);
        } else if self.links.len() < self.limit {
            if links.complete() {
                self.file(&links.allocation, true);
            }
            self.links.insert(key, links);
        } else {
            self.base.partial = true;
            self.base.upper =
                self.base.upper.saturating_add(links.allocation.bytes);
        }
    }
}

const fn adjust(total: &mut u64, value: u64, add: bool) {
    *total = if add {
        total.saturating_add(value)
    } else {
        total.saturating_sub(value)
    };
}

const fn replace(total: &mut Estimate, before: Estimate, after: Estimate) {
    total.lower = total
        .lower
        .saturating_sub(before.lower)
        .saturating_add(after.lower);
    total.upper = total
        .upper
        .saturating_sub(before.upper)
        .saturating_add(after.upper);
}

/// Accepted non-overlapping aggregated subtrees are evaluated together, so complete
/// groups spanning separate marks receive credit once.
///
/// Callers cache this scan-derived answer; removal authorization must still be checked afresh.
pub fn estimate<'a>(nodes: impl IntoIterator<Item = Node<'a>>) -> Estimate {
    let mut total = Accumulator::new(super::MAX_CLONE_GROUPS);
    for node in nodes {
        total.merge(gather(node));
    }
    total.estimate()
}

fn gather(node: Node<'_>) -> Accumulator {
    if node.allocation().is_some_and(|info| info.followed) {
        return Accumulator::new(super::MAX_CLONE_GROUPS);
    }
    if !node.is_dir() {
        return Accumulator::leaf(node, super::MAX_CLONE_GROUPS);
    }
    work();
    // Aggregation retains a summary on any subtree whose metadata can
    // interact with another mark, including zero-weight hardlink aliases.
    // Ordinary subtrees need no second walk after a mark changes.
    if node.stored_sharing().is_none() {
        let mut total = Accumulator::new(super::MAX_CLONE_GROUPS);
        total.base = Estimate {
            lower: node.bytes,
            upper: node.bytes,
            partial: false,
        };
        return total;
    }
    let mut total = Accumulator::new(super::MAX_CLONE_GROUPS);
    for child in node.children() {
        if child.is_dir() {
            total.merge(gather(child));
        } else {
            total.add_leaf(child);
        }
    }
    total
}

#[cfg(test)]
thread_local! {
    /// Map entries visited, here and by `sharing::refresh`, which tests
    /// hold to a budget.
    pub(super) static WORK: std::cell::Cell<u64> =
        const { std::cell::Cell::new(0) };
}

#[cfg(not(test))]
pub(super) const fn work() {}

#[cfg(test)]
pub(super) fn work() {
    WORK.with(|work| work.set(work.get() + 1));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Draft, Tree};
    use crate::tree::{Metric, NodeKind, Seen, aggregate_deduped};

    fn file(
        name: &str,
        inode: u64,
        links: u64,
        clone: Option<CloneInfo>,
    ) -> Draft {
        let mut node = Draft::entry(name, NodeKind::File, 4096);
        node.inode =
            Some((1, std::num::NonZeroU64::new(inode).expect("inode")));
        node.allocation = Some(Box::new(Allocation {
            bytes: 4096,
            identity: Some((1, inode)),
            links,
            followed: false,
            clone,
        }));
        node
    }

    fn clone_info() -> CloneInfo {
        CloneInfo {
            device: 1,
            id: 42,
            references: 2,
            private_bytes: Some(0),
            data_bytes: 4096,
        }
    }

    #[test]
    fn ten_worktrees_with_external_clones_free_only_their_private_files() {
        const MIB: u64 = 1024 * 1024;
        const LISTED: u64 = 10 * 1024 * MIB;
        let mut root = Draft::directory("root");
        for inode in 1..=11 {
            let mut dir = Draft::directory(format!("worktree-{inode}"));
            let mut shared = file(
                "shared",
                inode,
                1,
                Some(CloneInfo {
                    references: 11,
                    data_bytes: LISTED - 50 * MIB,
                    ..clone_info()
                }),
            );
            shared.bytes = LISTED - 50 * MIB;
            shared.allocation.as_mut().expect("allocation").bytes =
                shared.bytes;
            dir.children.push(shared);
            if inode <= 10 {
                dir.children.push(Draft::entry(
                    "private",
                    NodeKind::File,
                    50 * MIB,
                ));
            }
            root.children.push(dir);
        }
        let mut tree = Tree::from(root);
        aggregate_deduped(&mut tree, Metric::Bytes, &Seen::new());
        let root = tree.root();
        let selected = || root.children().iter().take(10);
        assert_eq!(selected().map(|n| n.bytes).sum::<u64>(), 100 * 1024 * MIB);
        assert_eq!(
            estimate(selected()),
            Estimate {
                lower: 500 * MIB,
                upper: 500 * MIB,
                partial: false,
            }
        );
        // Including the surviving member completes the union exactly once.
        assert_eq!(estimate(root.children()).lower, LISTED + 450 * MIB);
        assert_eq!(estimate(root.children()).upper, LISTED + 450 * MIB);
    }

    #[test]
    fn clone_reference_counts_use_the_largest_observation_and_volume() {
        let a = file("a", 1, 1, Some(clone_info()));
        let mut info = clone_info();
        info.references = 3;
        let b = file("b", 2, 1, Some(info));
        assert_eq!(
            estimate([Tree::from(&a).root(), Tree::from(&b).root()]).lower,
            0
        );
        info.device = 2;
        info.references = 2;
        let c = file("c", 3, 1, Some(info));
        assert_eq!(
            estimate([Tree::from(&a).root(), Tree::from(&c).root()]).lower,
            0
        );
    }

    #[test]
    fn selected_aliases_free_nothing_until_the_last_real_name_is_selected() {
        let mut root = Draft::directory("root");
        for name in ["a", "b"] {
            let mut dir = Draft::directory(name);
            dir.children.push(file("file", 1, 2, None));
            root.children.push(dir);
        }
        let mut tree = Tree::from(root);
        aggregate_deduped(&mut tree, Metric::Bytes, &Seen::new());
        let root = tree.root();
        assert_eq!(root.bytes, 4096);
        for child in root.children() {
            assert_eq!(estimate([child]).lower, 0);
            assert_eq!(child.sharing().reclaimable(child.bytes), 0);
        }
        assert_eq!(estimate(root.children()).lower, 4096);
        assert_eq!(root.sharing().reclaimable(root.bytes), 4096);
    }

    #[test]
    fn hardlinks_are_one_clone_reference_even_across_marked_directories() {
        let mut root = Draft::directory("root");
        for (name, inode, links) in [("a", 1, 2), ("b", 1, 2), ("c", 2, 1)] {
            let mut dir = Draft::directory(name);
            dir.children
                .push(file("file", inode, links, Some(clone_info())));
            root.children.push(dir);
        }
        let mut tree = Tree::from(root);
        aggregate_deduped(&mut tree, Metric::Bytes, &Seen::new());
        let root = tree.root();
        assert_eq!(root.bytes, 8192);
        assert_eq!(root.sharing().duplicate_bytes, 4096);
        assert_eq!(estimate(root.children().iter().take(2)).lower, 0);
        assert_eq!(estimate(root.children()).lower, 4096);
        assert_eq!(root.sharing().reclaimable(root.bytes), 4096);
        assert_eq!(estimate(root.children()).upper, 4096);
    }

    #[test]
    fn missing_identity_and_private_metadata_never_prove_a_complete_group() {
        for missing in ["device", "id", "private", "references"] {
            let mut info = clone_info();
            match missing {
                "device" => info.device = 0,
                "id" => info.id = 0,
                "private" => info.private_bytes = None,
                _ => info.references = 0,
            }
            let a = file("a", 1, 1, Some(info));
            let b = file("b", 2, 1, Some(info));
            let estimate =
                estimate([Tree::from(&a).root(), Tree::from(&b).root()]);
            assert_eq!(estimate.lower, 0, "{missing}");
            assert_eq!(estimate.upper, 8192, "{missing}");
            assert!(estimate.partial, "{missing}");
        }
    }

    #[test]
    fn modified_groups_and_unknown_sharing_supply_bounds_not_exact_savings() {
        let mut info = clone_info();
        info.private_bytes = Some(1024);
        let a = file("a", 1, 1, Some(info));
        let b = file("b", 2, 1, Some(info));
        assert_eq!(estimate([Tree::from(&a).root()]).lower, 1024);
        assert_eq!(estimate([Tree::from(&a).root()]).upper, 4096);
        assert_eq!(
            estimate([Tree::from(&a).root(), Tree::from(&b).root()]).lower,
            5120
        );
        assert_eq!(
            estimate([Tree::from(&a).root(), Tree::from(&b).root()]).upper,
            8192
        );
    }

    #[test]
    fn followed_links_cannot_credit_the_target_or_complete_a_clone_group() {
        let mut link = file("link", 1, 1, Some(clone_info()));
        link.allocation.as_mut().expect("allocation").followed = true;
        let real = file("real", 2, 1, Some(clone_info()));
        let mut directory_link = Draft::directory("directory-link");
        directory_link.allocation = link.allocation.clone();
        directory_link.children.push(real.clone());
        assert_eq!(
            estimate([Tree::from(&link).root(), Tree::from(&real).root()])
                .lower,
            0
        );
        assert_eq!(estimate([Tree::from(&directory_link).root()]).lower, 0);
    }

    #[test]
    fn bounded_grouping_drops_shared_credit_without_losing_private_bytes() {
        let mut total = Accumulator::new(1);
        for group in [1, 2] {
            for inode in [1, 2] {
                let mut info = clone_info();
                info.id = group;
                info.private_bytes = Some(1024);
                total.merge(Accumulator::leaf(
                    Tree::from(file("f", inode, 1, Some(info))).root(),
                    1,
                ));
            }
        }
        let estimate = total.estimate();
        assert!(estimate.partial);
        assert_eq!(estimate.lower, 4096);
        assert!(estimate.upper >= estimate.lower);
        assert!(total.groups.len() <= 1);
    }

    #[test]
    fn selected_set_work_scales_with_members_even_when_groups_and_marks_grow() {
        let mut counts = Vec::new();
        for n in [128, 1024] {
            let mut nodes = Vec::new();
            for group in 1..=n {
                for alias in [0, 1] {
                    let mut dir = Draft::directory(format!("g{group}-{alias}"));
                    let mut info = clone_info();
                    info.id = group;
                    for member in [1, 2] {
                        dir.children.push(file(
                            "f",
                            group * 2 + member,
                            2,
                            Some(info),
                        ));
                    }
                    let mut dir = Tree::from(dir);
                    aggregate_deduped(&mut dir, Metric::Bytes, &Seen::new());
                    nodes.push(dir);
                }
            }
            WORK.set(0);
            let value = estimate(nodes.iter().map(Tree::root));
            assert_eq!(value.lower, n * 4096);
            let work = WORK.get();
            assert!(work < n * 40, "absolute work budget: {n}: {work}");
            counts.push(work);
        }
        assert!(counts[1] <= counts[0] * 16, "{counts:?}");
        eprintln!(
            "selected-set accounting operations 128->1024 groups: {counts:?}"
        );
    }
}

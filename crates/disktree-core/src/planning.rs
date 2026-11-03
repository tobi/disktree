//! Read-only hypothetical path sets over one completed scan. No cart or intent.

use std::path::{Path, PathBuf};

use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};

use crate::removal::{self, Target};
use crate::scan::ScanOptions;
use crate::sharing::{self, Estimate};
use crate::space::{self, Attribution};
use crate::tree::{Node, Tree};

/// Bound request work and response size independently of the file count.
pub const MAX_PATHS: usize = 200;
pub const MAX_PEERS: usize = 20;
/// Peer discovery is optional and may be incomplete on very large clone trees.
const PEER_VISITS: usize = 2_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Context {
    pub root: PathBuf,
    pub scanned_at: i64,
    pub apparent_size: bool,
    pub clone_metadata: bool,
    pub include_hidden: bool,
    pub max_depth: Option<usize>,
    pub one_filesystem: bool,
    pub unreadable: u64,
}

impl Context {
    pub fn of(
        root: &Path,
        options: &ScanOptions,
        scanned_at: i64,
        unreadable: u64,
    ) -> Self {
        Self {
            root: root.to_path_buf(),
            scanned_at,
            apparent_size: options.apparent_size,
            clone_metadata: cfg!(target_os = "macos")
                && options.apfs_clone_metadata
                && !options.apparent_size,
            include_hidden: options.include_hidden,
            max_depth: options.max_depth,
            one_filesystem: options.one_filesystem,
            unreadable,
        }
    }

    const fn physical(&self) -> bool {
        !self.apparent_size && self.clone_metadata
    }

    const fn limited(&self) -> bool {
        self.unreadable > 0 || self.max_depth.is_some() || !self.include_hidden
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refused {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bounds {
    pub lower: u64,
    pub upper: u64,
    pub partial: bool,
}

impl From<Estimate> for Bounds {
    fn from(value: Estimate) -> Self {
        Self {
            lower: value.lower,
            upper: value.upper,
            partial: value.partial,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeAllocation {
    pub scanned_volume: u64,
    pub other_volumes: u64,
    pub unknown_volume: u64,
    /// Target-root attribution; nested volumes are not split by directory totals.
    pub scanned_volume_reclaim_lower: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub path: PathBuf,
    pub listed_bytes: u64,
    pub alone: Bounds,
    pub with_selection: Bounds,
    /// Interval subtraction, not the difference between two lower bounds.
    pub additional: Bounds,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peers {
    /// Direct child directories of this scope are the candidate units.
    pub scope: PathBuf,
    pub candidates: Vec<Peer>,
    pub candidates_found: usize,
    pub truncated: bool,
    pub nodes_visited: usize,
    pub node_budget: usize,
    pub selected_groups: usize,
    pub known_unselected_clone_files: u64,
    /// Only present after examining all relevant metadata in the scan.
    /// These are unresolved reference counts, not discovered owner paths.
    pub references_not_seen_in_scan: Option<u64>,
    pub with_all_returned_peers: Option<Bounds>,
    pub additional_with_all_returned_peers: Option<Bounds>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub context: Context,
    pub accepted: Vec<PathBuf>,
    pub covered: Vec<PathBuf>,
    pub refused: Vec<Refused>,
    /// Apparent lengths when `context.apparent_size`; allocation otherwise.
    pub listed_bytes: u64,
    pub private_allocation: Option<u64>,
    /// Listed clone allocation not known private, including repeated copies
    /// inside this very selection. It is not uniquely owned physical space.
    pub shared_allocation: Option<u64>,
    pub internal_duplicate_allocation: Option<u64>,
    pub reclaim: Option<Bounds>,
    pub volume: VolumeAllocation,
    pub peers: Option<Peers>,
    pub metadata_complete: bool,
    pub scan_complete: bool,
    pub limitations: Vec<String>,
}

/// A bounded path-list estimate.
///
/// Every accepted subtree comes from `tree`;
/// filesystem reads only apply the existing removal guards and attribution.
/// This creates no removal preview and does not authorize removal.
pub fn estimate(
    tree: &Tree,
    context: Context,
    paths: &[PathBuf],
    peer_scope: Option<&Path>,
    peer_limit: usize,
) -> Result<Report, String> {
    if paths.is_empty() || paths.len() > MAX_PATHS {
        return Err(format!("Supply 1 to {MAX_PATHS} paths."));
    }
    let mut refused = Vec::new();
    let mut targets = Vec::new();
    for path in paths {
        let path = removal::normalize(path);
        if let Some(node) = resolve(tree, &context.root, &path) {
            targets.push(Target {
                path,
                bytes: node.bytes,
                is_dir: node.is_dir(),
                hidden: false,
            });
        } else {
            refused.push(Refused {
                path,
                reason: "Not in this scanned tree (outside root, skipped, new or unreadable).".into(),
            });
        }
    }
    let mut plan = removal::plan(&targets, &context.root);
    refused.extend(plan.blocked.iter().map(|blocked| Refused {
        path: blocked.path.clone(),
        reason: blocked.reason.clone(),
    }));
    let nodes: Vec<_> = plan
        .targets
        .iter()
        .filter_map(|target| resolve(tree, &context.root, &target.path))
        .collect();
    let accounting = sharing::Selected::new(nodes.iter().copied());
    plan.estimate_selection(|targets| {
        let mut value = if targets.len() == nodes.len() {
            accounting.estimate()
        } else {
            sharing::estimate(targets.iter().filter_map(|target| {
                resolve(tree, &context.root, &target.path)
            }))
        };
        value.partial |= context.limited();
        value
    });
    let measured = Estimate {
        lower: plan.reclaimable_bytes(),
        upper: plan.possible_bytes(),
        partial: plan.estimate_partial(),
    };
    let shared = nodes
        .iter()
        .map(|node| node.sharing().shared_bytes().min(node.bytes))
        .sum::<u64>();
    // Cross-target duplicates need one union summary, not a sum of directory
    // summaries. The discovery pass below accounts for clone identities once.
    let duplicates = physical_duplicates(&nodes, context.physical());
    let physical = context.physical();
    let unsplit_volumes = space::mount_boundaries_for(&context.root)
        .is_some_and(|mounts| {
            plan.targets.iter().all(|target| {
                !target.is_dir
                    || !mounts
                        .iter()
                        .any(|mount| mount.starts_with(&target.path))
            })
        });
    let mut volume = VolumeAllocation {
        scanned_volume: 0,
        other_volumes: 0,
        unknown_volume: 0,
        scanned_volume_reclaim_lower: (physical
            && context.one_filesystem
            && unsplit_volumes)
            .then_some(plan.reclaim()),
    };
    for (target, attribution) in plan.attributed_targets() {
        match attribution {
            Attribution::Scanned => volume.scanned_volume += target.bytes,
            Attribution::Other => volume.other_volumes += target.bytes,
            Attribution::Unknown => volume.unknown_volume += target.bytes,
        }
    }
    let mut limitations = vec![
        "Scan-derived bounds before snapshots, open files, compression and later changes; actual freed space is not exactly predictable. Trash usually frees space only after it is emptied.".into(),
        "Volume attribution uses each accepted path root; directory totals can span nested volumes, so no scanned-volume reclaim is projected with cross-filesystem scans, nested mounts or unknown mount boundaries. Shared allocation includes internal duplicate listings and unknown private allocation; it is not unique physical usage or allocation necessarily retained outside the selection.".into(),
    ];
    if !physical {
        limitations.push("Physical bounds and private/shared allocation are unavailable: apparent sizes or APFS clone metadata disabled/unsupported. Use an allocated-size scan with clone collection on macOS.".into());
    }
    if measured.partial || duplicates.is_none() || context.limited() {
        limitations.push("Incomplete scan or allocation metadata: bounds cover observed entries only; omitted entries and unknown sharing can change the result.".into());
    }
    let peers = if peer_limit == 0 || !physical {
        None
    } else {
        let scope = peer_scope.map_or_else(
            || default_scope(&context.root, &plan.targets),
            removal::normalize,
        );
        let scope_node = resolve(tree, &context.root, &scope)
            .filter(|node| node.is_dir())
            .ok_or_else(|| {
                "Peer scope must be a directory in this scan.".to_string()
            })?;
        Some(peers(
            tree,
            &context,
            &nodes,
            &accounting,
            &plan.targets,
            &scope,
            scope_node,
            peer_limit.min(MAX_PEERS),
        ))
    };
    Ok(Report {
        listed_bytes: plan.bytes(),
        private_allocation: physical
            .then_some(plan.bytes().saturating_sub(shared)),
        shared_allocation: physical.then_some(shared),
        internal_duplicate_allocation: physical.then_some(duplicates).flatten(),
        reclaim: physical.then_some(measured.into()),
        volume,
        metadata_complete: physical
            && !measured.partial
            && duplicates.is_some(),
        scan_complete: !context.limited(),
        context,
        accepted: plan
            .targets
            .iter()
            .map(|target| target.path.clone())
            .collect(),
        covered: plan
            .covered
            .iter()
            .map(|target| target.path.clone())
            .collect(),
        refused,
        peers,
        limitations,
    })
}

fn resolve<'a>(tree: &'a Tree, root: &Path, path: &Path) -> Option<Node<'a>> {
    tree.root()
        .descend(path.strip_prefix(root).ok()?)
        .map(|(_, node)| node)
}

fn default_scope(root: &Path, targets: &[Target]) -> PathBuf {
    let mut scope = targets
        .first()
        .and_then(|target| target.path.parent())
        .unwrap_or(root);
    for target in targets {
        while !target.path.starts_with(scope) {
            scope = scope.parent().unwrap_or(root);
        }
    }
    scope.to_path_buf()
}

type Key = (u64, u64);

/// An iterative walk, pruning ordinary subtrees and never allocating paths
/// per file. The budget also bounds the iterator stack on a very deep tree.
fn visit<'a>(
    node: Node<'a>,
    remaining: &mut usize,
    mut leaf: impl FnMut(Node<'a>),
) -> bool {
    if node
        .allocation()
        .is_some_and(|allocation| allocation.followed)
    {
        return true;
    }
    let mut stack = vec![node.children().iter()];
    if !node.is_dir() {
        if *remaining == 0 {
            return false;
        }
        *remaining -= 1;
        leaf(node);
        return true;
    }
    while let Some(children) = stack.last_mut() {
        let Some(child) = children.next() else {
            stack.pop();
            continue;
        };
        if *remaining == 0 {
            return false;
        }
        *remaining -= 1;
        if child.allocation().is_some_and(|info| info.followed) {
            continue;
        }
        if child.is_dir() {
            if child.stored_sharing().is_some() {
                stack.push(child.children().iter());
            }
        } else {
            leaf(child);
        }
    }
    true
}

fn physical_duplicates(nodes: &[Node<'_>], physical: bool) -> Option<u64> {
    if !physical {
        return None;
    }
    let mut summary = sharing::DuplicateSummary::default();
    let mut remaining = PEER_VISITS;
    for node in nodes {
        if !visit(*node, &mut remaining, |leaf| summary.add(leaf)) {
            return None;
        }
    }
    summary.bytes()
}

#[derive(Default)]
struct Group {
    references: u32,
    seen: u64,
    selected: u64,
}

#[allow(
    clippy::too_many_arguments,
    reason = "All inputs are borrowed from one scan and selection."
)]
fn peers<'a>(
    tree: &'a Tree,
    context: &Context,
    selected: &[Node<'a>],
    accounting: &sharing::Selected,
    targets: &[Target],
    scope: &Path,
    scope_node: Node<'a>,
    limit: usize,
) -> Peers {
    let mut groups: FxHashMap<Key, Group> = FxHashMap::default();
    let mut remaining = PEER_VISITS;
    let mut complete = true;
    let mut selected_links = FxHashSet::default();
    for node in selected {
        complete &= visit(*node, &mut remaining, |leaf| {
            let Some(allocation) = leaf.allocation() else {
                return;
            };
            let Some(info) = allocation.clone else {
                return;
            };
            if info.device == 0 || info.id == 0 || info.references < 2 {
                return;
            }
            if allocation.links > 1
                && allocation
                    .identity
                    .is_some_and(|id| !selected_links.insert(id))
            {
                return;
            }
            let group = groups.entry((info.device, info.id)).or_default();
            group.references = group.references.max(info.references);
            group.selected += 1;
        });
    }
    let mut base = accounting.estimate();
    base.partial |= context.limited();
    if groups.is_empty() {
        return Peers {
            scope: scope.to_path_buf(),
            candidates: Vec::new(),
            candidates_found: 0,
            truncated: !complete,
            nodes_visited: PEER_VISITS - remaining,
            node_budget: PEER_VISITS,
            selected_groups: 0,
            known_unselected_clone_files: 0,
            references_not_seen_in_scan: (complete
                && !context.limited()
                && !base.partial)
                .then_some(0),
            with_all_returned_peers: Some(base.into()),
            additional_with_all_returned_peers: Some(delta(base, base)),
        };
    }
    let mut seen_links = FxHashSet::default();
    complete &= visit(tree.root(), &mut remaining, |leaf| {
        let Some(allocation) = leaf.allocation() else {
            return;
        };
        let Some(info) = allocation.clone else {
            return;
        };
        let Some(group) = groups.get_mut(&(info.device, info.id)) else {
            return;
        };
        if allocation.links > 1
            && allocation.identity.is_some_and(|id| !seen_links.insert(id))
        {
            return;
        }
        group.references = group.references.max(info.references);
        group.seen += 1;
    });
    // Candidate units are direct children of scope. Resolve exclusions once,
    // rather than compare every directory against every selected path.
    let scope_selected =
        targets.iter().any(|target| scope.starts_with(&target.path));
    let excluded: FxHashSet<_> = targets
        .iter()
        .filter_map(|target| {
            target
                .path
                .strip_prefix(scope)
                .ok()?
                .components()
                .next()
                .map(|component| component.as_os_str().to_os_string())
        })
        .collect();
    let mut candidates = Vec::new();
    let mut candidates_found = 0;
    for child in scope_node.children() {
        if remaining == 0 {
            complete = false;
            break;
        }
        remaining -= 1;
        if !child.is_dir() || child.stored_sharing().is_none() {
            continue;
        }
        if scope_selected
            || excluded.contains(std::ffi::OsStr::new(child.name()))
        {
            continue;
        }
        let mut related = false;
        complete &= visit(child, &mut remaining, |leaf| {
            related |=
                leaf.allocation().and_then(|a| a.clone).is_some_and(|info| {
                    groups.contains_key(&(info.device, info.id))
                });
        });
        if related {
            candidates_found += 1;
            if candidates.len() < limit {
                candidates.push((scope.join(child.name()), child));
            }
        }
    }
    // Only the bounded returned directories are checked by the removal guards.
    let candidate_targets: Vec<_> = candidates
        .iter()
        .map(|(path, node)| Target {
            path: path.clone(),
            bytes: node.bytes,
            is_dir: true,
            hidden: false,
        })
        .collect();
    let allowed = removal::plan(&candidate_targets, &context.root);
    candidates
        .retain(|(path, _)| allowed.targets.iter().any(|t| t.path == *path));
    let nodes: Vec<_> = candidates.iter().map(|(_, node)| *node).collect();
    let (answers, mut combined) = accounting.additions(&nodes);
    combined.partial |= context.limited();
    let candidates = candidates
        .into_iter()
        .zip(answers)
        .map(|((path, node), (mut alone, mut union))| {
            alone.partial |= context.limited();
            union.partial |= context.limited();
            Peer {
                path,
                listed_bytes: node.bytes,
                alone: alone.into(),
                with_selection: union.into(),
                additional: delta(base, union),
            }
        })
        .collect::<Vec<_>>();
    let truncated = !complete || candidates_found > candidates.len();
    Peers {
        scope: scope.to_path_buf(),
        candidates,
        candidates_found,
        truncated,
        nodes_visited: PEER_VISITS - remaining,
        node_budget: PEER_VISITS,
        selected_groups: groups.len(),
        known_unselected_clone_files: groups
            .values()
            .map(|g| g.seen.saturating_sub(g.selected))
            .sum(),
        references_not_seen_in_scan: (complete
            && !context.limited()
            && !base.partial
            && groups.values().all(|g| g.seen <= u64::from(g.references)))
        .then(|| {
            groups
                .values()
                .map(|g| u64::from(g.references).saturating_sub(g.seen))
                .sum()
        }),
        with_all_returned_peers: Some(combined.into()),
        additional_with_all_returned_peers: Some(delta(base, combined)),
    }
}

const fn delta(base: Estimate, union: Estimate) -> Bounds {
    Bounds {
        lower: union.lower.saturating_sub(base.upper),
        upper: union.upper.saturating_sub(base.lower),
        partial: base.partial || union.partial,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sharing::{Allocation, CloneInfo};
    use crate::tree::{Draft, Metric, NodeKind, Seen, aggregate_deduped};

    const GB: u64 = 1_000_000_000;
    const SIZE: u64 = 12 * GB;

    fn clone_file(inode: u64, refs: u32, private: Option<u64>) -> Draft {
        let mut file = Draft::entry("data", NodeKind::File, SIZE);
        file.inode =
            Some((1, std::num::NonZeroU64::new(inode).expect("inode")));
        file.allocation = Some(Box::new(Allocation {
            bytes: SIZE,
            identity: Some((1, inode)),
            links: 1,
            followed: false,
            clone: Some(CloneInfo {
                device: 1,
                id: 42,
                references: refs,
                private_bytes: private,
                data_bytes: SIZE,
            }),
        }));
        file
    }

    fn fixture(
        refs: u32,
        private: Option<u64>,
    ) -> (tempfile::TempDir, Tree, Context) {
        let temp = tempfile::tempdir().expect("fixture");
        let mut root = Draft::directory("root");
        for index in 1..=4 {
            let name = format!("worktree-{index}");
            std::fs::create_dir(temp.path().join(&name)).expect("directory");
            let mut dir = Draft::directory(name);
            dir.children.push(clone_file(index, refs, private));
            root.children.push(dir);
        }
        let mut tree = Tree::from(root);
        aggregate_deduped(&mut tree, Metric::Bytes, &Seen::new());
        let context = Context {
            clone_metadata: true,
            ..Context::of(temp.path(), &ScanOptions::default(), 123, 0)
        };
        (temp, tree, context)
    }

    fn selected(root: &Path, count: u64) -> Vec<PathBuf> {
        (1..=count)
            .map(|i| root.join(format!("worktree-{i}")))
            .collect()
    }

    #[test]
    fn three_of_four_12_gb_worktrees_find_the_last_holder_and_unlock_once() {
        let (temp, tree, context) = fixture(4, Some(0));
        let report = estimate(
            &tree,
            context.clone(),
            &selected(temp.path(), 3),
            None,
            10,
        )
        .expect("estimate");
        assert_eq!(report.listed_bytes, 36 * GB);
        assert_eq!(report.private_allocation, Some(0));
        assert_eq!(report.shared_allocation, Some(36 * GB));
        assert_eq!(report.internal_duplicate_allocation, Some(24 * GB));
        assert_eq!(
            report.reclaim.as_ref().map(|b| (b.lower, b.upper)),
            Some((0, 0))
        );
        let peers = report.peers.expect("peers");
        assert_eq!(peers.known_unselected_clone_files, 1);
        assert_eq!(peers.references_not_seen_in_scan, Some(0));
        assert_eq!(peers.candidates.len(), 1);
        let peer = &peers.candidates[0];
        assert_eq!(peer.path, temp.path().join("worktree-4"));
        assert_eq!(
            (peer.additional.lower, peer.additional.upper),
            (SIZE, SIZE)
        );
        assert_eq!(peer.with_selection.lower, SIZE);
        let report =
            estimate(&tree, context, &selected(temp.path(), 4), None, 10)
                .expect("all four");
        assert_eq!(report.listed_bytes, 48 * GB);
        assert_eq!(report.internal_duplicate_allocation, Some(36 * GB));
        assert_eq!(
            report.reclaim.as_ref().map(|b| (b.lower, b.upper)),
            Some((SIZE, SIZE))
        );
        assert!(report.peers.expect("peers").candidates.is_empty());
    }

    #[test]
    fn internal_duplicates_overlap_and_refusals_do_not_inflate_union() {
        let (temp, tree, context) = fixture(4, Some(0));
        let mut paths = selected(temp.path(), 4);
        paths.extend([
            paths[0].clone(),
            paths[0].join("data"),
            temp.path().to_path_buf(),
            temp.path().join("missing"),
            temp.path().parent().expect("parent").join("outside"),
        ]);
        let report =
            estimate(&tree, context, &paths, None, 0).expect("estimate");
        assert_eq!(report.accepted.len(), 4);
        assert_eq!(report.covered.len(), 2);
        assert_eq!(report.refused.len(), 3);
        assert_eq!(report.reclaim.expect("bounds").lower, SIZE);
        assert_eq!(report.internal_duplicate_allocation, Some(36 * GB));
    }

    #[test]
    fn external_references_have_no_invented_owner_paths() {
        let (temp, tree, context) = fixture(5, Some(0));
        let report = estimate(
            &tree,
            context.clone(),
            &selected(temp.path(), 3),
            None,
            10,
        )
        .expect("estimate");
        let peers = report.peers.expect("peers");
        assert_eq!(peers.references_not_seen_in_scan, Some(1));
        assert_eq!(peers.known_unselected_clone_files, 1);
        assert_eq!(peers.candidates[0].with_selection.upper, 0);
        let limited = Context {
            include_hidden: false,
            ..context
        };
        let report =
            estimate(&tree, limited, &selected(temp.path(), 3), None, 10)
                .expect("limited");
        assert_eq!(
            report.peers.expect("peers").references_not_seen_in_scan,
            None
        );
        assert!(!report.scan_complete);
    }

    #[test]
    fn modified_clones_keep_uncertain_bounds_and_interval_deltas() {
        let (temp, tree, context) = fixture(4, Some(GB));
        let report =
            estimate(&tree, context, &selected(temp.path(), 3), None, 10)
                .expect("estimate");
        assert_eq!(report.private_allocation, Some(3 * GB));
        assert_eq!(report.shared_allocation, Some(33 * GB));
        assert_eq!(
            report.reclaim.map(|b| (b.lower, b.upper)),
            Some((3 * GB, 36 * GB))
        );
        let peers = report.peers.expect("peers");
        assert_eq!(peers.candidates[0].with_selection.lower, 15 * GB);
        assert_eq!(peers.candidates[0].with_selection.upper, 48 * GB);
        assert_eq!(peers.candidates[0].additional.lower, 0);
        assert_eq!(peers.candidates[0].additional.upper, 45 * GB);
    }

    #[test]
    fn missing_metadata_and_unavailable_collection_are_not_exact_measurements()
    {
        let (temp, tree, context) = fixture(4, None);
        let report = estimate(
            &tree,
            context.clone(),
            &selected(temp.path(), 4),
            None,
            10,
        )
        .expect("missing private");
        assert!(report.reclaim.expect("wide bounds").partial);
        assert!(!report.metadata_complete);
        assert_eq!(
            report.peers.expect("peers").references_not_seen_in_scan,
            None
        );
        for context in [
            Context {
                clone_metadata: false,
                ..context.clone()
            },
            Context {
                apparent_size: true,
                ..context
            },
        ] {
            let report =
                estimate(&tree, context, &selected(temp.path(), 4), None, 10)
                    .expect("unavailable");
            assert!(report.reclaim.is_none());
            assert!(report.private_allocation.is_none());
            assert!(report.shared_allocation.is_none());
            assert!(report.peers.is_none());
        }
    }

    #[test]
    fn hardlink_aliases_are_not_extra_clone_members_or_reclaim() {
        let (temp, _, context) = fixture(2, Some(0));
        let mut root = Draft::directory("root");
        for (index, inode, links) in [(1, 1, 2), (2, 1, 2), (3, 2, 1)] {
            let mut dir = Draft::directory(format!("worktree-{index}"));
            let mut file = clone_file(inode, 2, Some(0));
            file.allocation.as_mut().expect("allocation").links = links;
            dir.children.push(file);
            root.children.push(dir);
        }
        let mut tree = Tree::from(root);
        aggregate_deduped(&mut tree, Metric::Bytes, &Seen::new());
        let report =
            estimate(&tree, context, &selected(temp.path(), 2), None, 10)
                .expect("aliases");
        assert_eq!(report.listed_bytes, SIZE);
        assert_eq!(report.internal_duplicate_allocation, Some(0));
        let peers = report.peers.expect("peers");
        assert_eq!(peers.known_unselected_clone_files, 1);
        assert_eq!(peers.references_not_seen_in_scan, Some(0));
        assert_eq!(peers.candidates[0].with_selection.lower, SIZE);
    }

    #[test]
    fn clone_ids_on_separate_volumes_do_not_complete_each_other() {
        let (temp, _, mut context) = fixture(2, Some(0));
        let mut root = Draft::directory("root");
        for device in [1, 2] {
            let mut dir = Draft::directory(format!("worktree-{device}"));
            let mut file = clone_file(device, 2, Some(0));
            file.allocation
                .as_mut()
                .expect("allocation")
                .clone
                .as_mut()
                .expect("clone")
                .device = device;
            dir.children.push(file);
            root.children.push(dir);
        }
        let mut tree = Tree::from(root);
        aggregate_deduped(&mut tree, Metric::Bytes, &Seen::new());
        context.one_filesystem = false;
        let report =
            estimate(&tree, context, &selected(temp.path(), 2), None, 10)
                .expect("volumes");
        assert_eq!(report.reclaim.expect("bounds").upper, 0);
        assert_eq!(report.internal_duplicate_allocation, Some(0));
        assert!(report.volume.scanned_volume_reclaim_lower.is_none());
        assert_eq!(
            report.peers.expect("peers").references_not_seen_in_scan,
            Some(2)
        );
    }

    #[test]
    fn bounded_peers_evaluate_the_returned_union_instead_of_summing_rows() {
        let (temp, tree, context) = fixture(4, Some(0));
        let report = estimate(
            &tree,
            context.clone(),
            &selected(temp.path(), 1),
            None,
            2,
        )
        .expect("bounded");
        let peers = report.peers.expect("peers");
        assert_eq!(peers.candidates.len(), 2);
        assert_eq!(peers.candidates_found, 3);
        assert!(peers.truncated);
        assert_eq!(peers.with_all_returned_peers.expect("union").upper, 0);
        let report =
            estimate(&tree, context, &selected(temp.path(), 1), None, 3)
                .expect("all peers");
        let peers = report.peers.expect("peers");
        assert!(peers.candidates.iter().all(|p| p.additional.upper == 0));
        assert_eq!(
            peers
                .additional_with_all_returned_peers
                .expect("union")
                .lower,
            SIZE
        );
    }

    #[test]
    fn peer_scope_can_find_a_known_holder_outside_the_default_parent() {
        let temp = tempfile::tempdir().expect("fixture");
        let mut root = Draft::directory("root");
        let mut near = Draft::directory("near");
        for inode in 1..=3 {
            let name = format!("worktree-{inode}");
            std::fs::create_dir_all(temp.path().join("near").join(&name))
                .expect("near");
            let mut dir = Draft::directory(name);
            dir.children.push(clone_file(inode, 4, Some(0)));
            near.children.push(dir);
        }
        let mut distant = Draft::directory("distant");
        std::fs::create_dir(temp.path().join("distant")).expect("distant");
        distant.children.push(clone_file(4, 4, Some(0)));
        root.children.extend([near, distant]);
        let mut tree = Tree::from(root);
        aggregate_deduped(&mut tree, Metric::Bytes, &Seen::new());
        let context = Context {
            clone_metadata: true,
            ..Context::of(temp.path(), &ScanOptions::default(), 1, 0)
        };
        let paths = selected(&temp.path().join("near"), 3);
        let report = estimate(&tree, context.clone(), &paths, None, 10)
            .expect("default scope");
        let peers = report.peers.expect("peers");
        assert!(peers.candidates.is_empty());
        assert_eq!(peers.known_unselected_clone_files, 1);
        assert_eq!(peers.references_not_seen_in_scan, Some(0));
        let report = estimate(&tree, context, &paths, Some(temp.path()), 10)
            .expect("wider scope");
        let peers = report.peers.expect("peers");
        assert_eq!(peers.candidates[0].path, temp.path().join("distant"));
        assert_eq!(peers.candidates[0].additional.lower, SIZE);
    }

    #[test]
    fn peer_walk_is_bounded_and_prunes_large_ordinary_subtrees() {
        let mut root = Draft::directory("root");
        let mut ordinary = Draft::directory("ordinary");
        for index in 0..100_000 {
            ordinary.children.push(Draft::entry(
                index.to_string(),
                NodeKind::File,
                1,
            ));
        }
        root.children.push(ordinary);
        let mut tree = Tree::from(root);
        aggregate_deduped(&mut tree, Metric::Bytes, &Seen::new());
        let mut remaining = 100;
        assert!(visit(tree.root(), &mut remaining, |_| panic!(
            "ordinary subtree pruned"
        )));
        assert_eq!(remaining, 99);
        let mut remaining = 3;
        let ordinary = tree.root().children().iter().next().expect("ordinary");
        let mut leaves = 0;
        assert!(!visit(ordinary, &mut remaining, |_| leaves += 1));
        assert_eq!(leaves, 3);
        assert_eq!(remaining, 0);
    }
}

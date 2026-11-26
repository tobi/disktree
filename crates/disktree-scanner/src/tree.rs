//! Compact arena-backed filesystem tree.
//!
//! Every node is a flat 64-byte [`ArenaNode`] stored in a contiguous `Vec`,
//! with zero per-node heap allocations: names live in [`StringPool`] and
//! multi-link hardlink identities live in a sparse side table (so single-link
//! files pay 0 bytes for inode tracking).

use std::num::NonZeroU32;
use std::path::{Path, PathBuf};

use rustc_hash::FxHashSet;

use crate::strings::{NameId, StringPool};
use crate::types::{Metric, NodeKind, SubtreeStatus};

/// Dense 1-based node handle. Using `NonZeroU32` gives `Option<NodeId>`
/// niche optimization so every parent/child/sibling pointer is 4 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(NonZeroU32);

impl NodeId {
    pub const ROOT: Self = Self(NonZeroU32::MIN);

    pub(crate) fn from_index(index: usize) -> Option<Self> {
        let one_based = u32::try_from(index.checked_add(1)?).ok()?;
        NonZeroU32::new(one_based).map(Self)
    }

    pub const fn index(self) -> usize {
        (self.0.get() - 1) as usize
    }
}

/// Bit-packed node flags (4 bytes).
///
/// Layout:
/// - bits 0..2: `NodeKind`
/// - bit 2: `read_error`
/// - bit 3: `mount_boundary`
/// - bit 4: `lossy_name` (non-UTF-8 name replaced with U+FFFD)
/// - bit 5: `hidden` (leading dot or OS hidden attribute)
/// - bit 6: `has_hardlink` (present in `TreeArena::hardlinks`)
/// - bit 7: `external_shard_ref` (serialization marker for child shards)
/// - bits 8..10: `SubtreeStatus` tag
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NodeFlags(pub u32);

impl NodeFlags {
    pub const READ_ERROR: u32 = 1 << 2;
    pub const MOUNT_BOUNDARY: u32 = 1 << 3;
    pub const LOSSY_NAME: u32 = 1 << 4;
    pub const HIDDEN: u32 = 1 << 5;
    pub const HAS_HARDLINK: u32 = 1 << 6;
    pub const EXTERNAL_SHARD_REF: u32 = 1 << 7;
    const STATUS_SHIFT: u32 = 8;
    const STATUS_MASK: u32 = 0b11 << Self::STATUS_SHIFT;
    /// Set by `aggregate()` on duplicate hardlink leaves so their bytes are
    /// charged once without destroying their true file size across shard reloads.
    pub const DEDUPED_LINK: u32 = 1 << 10;

    pub const fn new(kind: NodeKind) -> Self {
        Self(kind.to_bits())
    }

    pub const fn kind(self) -> NodeKind {
        NodeKind::from_bits(self.0)
    }

    pub const fn set(&mut self, bit: u32, value: bool) {
        if value {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }

    pub const fn has(self, bit: u32) -> bool {
        (self.0 & bit) != 0
    }

    pub const fn status_tag(self) -> u32 {
        (self.0 & Self::STATUS_MASK) >> Self::STATUS_SHIFT
    }

    pub const fn set_status_tag(&mut self, tag: u32) {
        self.0 = (self.0 & !Self::STATUS_MASK)
            | ((tag & 0b11) << Self::STATUS_SHIFT);
    }

    /// Flags stripped of transient serialization/aggregation bits.
    pub const fn persistent_bits(self) -> u32 {
        self.0
            & !(Self::EXTERNAL_SHARD_REF
                | Self::STATUS_MASK
                | Self::DEDUPED_LINK)
    }
}

/// One filesystem entry in a [`TreeArena`].
///
/// Kept to a single 64-byte cache line with zero heap allocations per node.
/// Leaf `allocated_bytes` and `apparent_bytes` are the sole size inputs;
/// directory totals are derived by [`TreeArena::aggregate`].
#[derive(Clone, Copy, Debug)]
pub struct ArenaNode {
    pub name: NameId,
    pub parent: Option<NodeId>,
    pub first_child: Option<NodeId>,
    pub next_sibling: Option<NodeId>,
    pub allocated_bytes: u64,
    pub apparent_bytes: u64,
    pub files: u64,
    pub dirs: u32,
    pub flags: NodeFlags,
    pub modified: i64,
    pub scanned_at: u64,
}

impl ArenaNode {
    pub const fn is_dir(&self) -> bool {
        self.flags.kind().is_dir()
    }

    pub const fn kind(&self) -> NodeKind {
        self.flags.kind()
    }

    pub const fn read_error(&self) -> bool {
        self.flags.has(NodeFlags::READ_ERROR)
    }

    pub const fn lossy_name(&self) -> bool {
        self.flags.has(NodeFlags::LOSSY_NAME)
    }

    pub const fn hidden(&self) -> bool {
        self.flags.has(NodeFlags::HIDDEN)
    }

    pub const fn status(&self) -> SubtreeStatus {
        SubtreeStatus::from_tag(self.flags.status_tag(), self.scanned_at)
    }

    pub const fn set_status(&mut self, status: SubtreeStatus) {
        self.flags.set_status_tag(status.tag());
        match status {
            SubtreeStatus::Fresh { scanned_at }
            | SubtreeStatus::Stale { scanned_at } => {
                self.scanned_at = scanned_at;
            }
            SubtreeStatus::Updating {
                previous_scanned_at,
            } => {
                self.scanned_at = match previous_scanned_at {
                    Some(ts) => ts,
                    None => 0,
                };
            }
            SubtreeStatus::Unscanned => {
                self.scanned_at = 0;
            }
        }
    }

    /// Allocated bytes charged to this node (0 for a deduplicated hardlink
    /// leaf whose inode was already charged elsewhere).
    pub const fn effective_allocated_bytes(&self) -> u64 {
        if !self.is_dir() && self.flags.has(NodeFlags::DEDUPED_LINK) {
            0
        } else {
            self.allocated_bytes
        }
    }

    /// Apparent bytes charged to this node (0 for a deduplicated hardlink leaf).
    pub const fn effective_apparent_bytes(&self) -> u64 {
        if !self.is_dir() && self.flags.has(NodeFlags::DEDUPED_LINK) {
            0
        } else {
            self.apparent_bytes
        }
    }

    pub const fn value(&self, metric: Metric) -> u64 {
        match metric {
            Metric::Bytes => self.effective_allocated_bytes(),
            Metric::ApparentBytes => self.effective_apparent_bytes(),
            Metric::Files => self.files,
        }
    }
}

/// Sparse hardlink record: only populated for files with `nlink > 1` (or
/// followed symlinks), so single-link files pay zero bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HardlinkEntry {
    pub node: NodeId,
    pub dev: u64,
    pub ino: u64,
}

/// Compact arena holding a scanned or cached filesystem subtree.
#[derive(Clone, Debug)]
pub struct TreeArena {
    pub root_path: PathBuf,
    nodes: Vec<ArenaNode>,
    /// Tracks last child per node during construction for O(1) append.
    last_child: Vec<Option<NodeId>>,
    strings: StringPool,
    hardlinks: Vec<HardlinkEntry>,
}

impl TreeArena {
    /// Create a new arena with a single directory root node.
    pub fn new(root_path: PathBuf, root_name: &str) -> Self {
        let mut strings = StringPool::new();
        let name = strings.push_raw(root_name);
        let root = ArenaNode {
            name,
            parent: None,
            first_child: None,
            next_sibling: None,
            allocated_bytes: 0,
            apparent_bytes: 0,
            files: 0,
            dirs: 1,
            flags: NodeFlags::new(NodeKind::Directory),
            modified: 0,
            scanned_at: 0,
        };
        Self {
            root_path,
            nodes: vec![root],
            last_child: vec![None],
            strings,
            hardlinks: Vec::new(),
        }
    }

    /// Drop build-time string deduplication tables to minimize resident memory
    /// once all nodes have been inserted into the arena.
    pub fn finish_building(&mut self) {
        self.strings.finish_building();
    }

    /// Create an arena with pre-allocated node and raw string capacity
    /// (zero build-time hash table, single backing allocation when exact).
    pub fn with_exact_capacity(
        root_path: PathBuf,
        root_name: &str,
        expected_nodes: usize,
        expected_string_bytes: usize,
    ) -> Self {
        let mut strings = StringPool::with_capacity(
            expected_nodes.max(1),
            expected_string_bytes.max(root_name.len()),
        );
        let name = strings.push_raw(root_name);
        let cap = expected_nodes.max(1);
        let mut nodes = Vec::with_capacity(cap);
        let mut last_child = Vec::with_capacity(cap);
        nodes.push(ArenaNode {
            name,
            parent: None,
            first_child: None,
            next_sibling: None,
            allocated_bytes: 0,
            apparent_bytes: 0,
            files: 0,
            dirs: 1,
            flags: NodeFlags::new(NodeKind::Directory),
            modified: 0,
            scanned_at: 0,
        });
        last_child.push(None);
        Self {
            root_path,
            nodes,
            last_child,
            strings,
            hardlinks: Vec::new(),
        }
    }

    /// Create an arena with pre-allocated capacity.
    pub fn with_capacity(
        root_path: PathBuf,
        root_name: &str,
        expected_nodes: usize,
    ) -> Self {
        let mut strings =
            StringPool::with_capacity(expected_nodes / 2, expected_nodes * 8);
        let name = strings.intern(root_name);
        let mut nodes = Vec::with_capacity(expected_nodes.max(1));
        let mut last_child = Vec::with_capacity(expected_nodes.max(1));
        nodes.push(ArenaNode {
            name,
            parent: None,
            first_child: None,
            next_sibling: None,
            allocated_bytes: 0,
            apparent_bytes: 0,
            files: 0,
            dirs: 1,
            flags: NodeFlags::new(NodeKind::Directory),
            modified: 0,
            scanned_at: 0,
        });
        last_child.push(None);
        Self {
            root_path,
            nodes,
            last_child,
            strings,
            hardlinks: Vec::new(),
        }
    }

    /// Build directly from decoded parts (used by Protobuf chunk decoding).
    pub(crate) const fn from_parts(
        root_path: PathBuf,
        nodes: Vec<ArenaNode>,
        last_child: Vec<Option<NodeId>>,
        strings: StringPool,
        hardlinks: Vec<HardlinkEntry>,
    ) -> Self {
        Self {
            root_path,
            nodes,
            last_child,
            strings,
            hardlinks,
        }
    }

    pub const fn root(&self) -> NodeId {
        NodeId::ROOT
    }

    pub const fn len(&self) -> usize {
        self.nodes.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn node(&self, id: NodeId) -> &ArenaNode {
        &self.nodes[id.index()]
    }

    pub fn node_mut(&mut self, id: NodeId) -> &mut ArenaNode {
        &mut self.nodes[id.index()]
    }

    pub fn nodes(&self) -> &[ArenaNode] {
        &self.nodes
    }

    pub const fn strings(&self) -> &StringPool {
        &self.strings
    }

    pub fn hardlinks(&self) -> &[HardlinkEntry] {
        &self.hardlinks
    }

    pub fn name(&self, id: NodeId) -> &str {
        self.strings.get(self.nodes[id.index()].name)
    }

    /// Add a child entry under `parent` and return its [`NodeId`].
    #[allow(clippy::too_many_arguments, reason = "flat leaf constructor")]
    pub fn add_child(
        &mut self,
        parent: NodeId,
        name: &str,
        kind: NodeKind,
        allocated_bytes: u64,
        apparent_bytes: u64,
        modified: i64,
        flags: NodeFlags,
    ) -> NodeId {
        let name_id = self.strings.intern(name);
        self.add_child_with_name_id(
            parent,
            name_id,
            kind,
            allocated_bytes,
            apparent_bytes,
            modified,
            flags,
        )
    }

    /// Hot-path child insertion that appends `name` straight into the flat
    /// string buffer without building or probing a hash table.
    #[allow(
        clippy::too_many_arguments,
        reason = "mirrors ArenaNode fields without extra wrapper structs"
    )]
    pub fn add_child_raw(
        &mut self,
        parent: NodeId,
        name: &str,
        kind: NodeKind,
        allocated_bytes: u64,
        apparent_bytes: u64,
        modified: i64,
        flags: NodeFlags,
    ) -> NodeId {
        let name_id = self.strings.push_raw(name);
        self.add_child_with_name_id(
            parent,
            name_id,
            kind,
            allocated_bytes,
            apparent_bytes,
            modified,
            flags,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "shared flat ArenaNode insertion helper"
    )]
    fn add_child_with_name_id(
        &mut self,
        parent: NodeId,
        name_id: NameId,
        kind: NodeKind,
        allocated_bytes: u64,
        apparent_bytes: u64,
        modified: i64,
        flags: NodeFlags,
    ) -> NodeId {
        let id = NodeId::from_index(self.nodes.len())
            .expect("arena node count fits in u32");
        let is_file = kind == NodeKind::File;
        let is_dir = kind == NodeKind::Directory;
        self.nodes.push(ArenaNode {
            name: name_id,
            parent: Some(parent),
            first_child: None,
            next_sibling: None,
            allocated_bytes,
            apparent_bytes,
            files: u64::from(is_file),
            dirs: u32::from(is_dir),
            flags,
            modified,
            scanned_at: 0,
        });
        self.last_child.push(None);
        self.link_child(parent, id);
        id
    }

    pub fn record_hardlink(&mut self, node: NodeId, dev: u64, ino: u64) {
        self.nodes[node.index()]
            .flags
            .set(NodeFlags::HAS_HARDLINK, true);
        self.hardlinks.push(HardlinkEntry { node, dev, ino });
    }

    pub(crate) fn link_child(&mut self, parent: NodeId, child: NodeId) {
        let p = parent.index();
        if let Some(prev) = self.last_child[p] {
            self.nodes[prev.index()].next_sibling = Some(child);
        } else {
            self.nodes[p].first_child = Some(child);
        }
        self.last_child[p] = Some(child);
    }

    /// Iterate over direct children of `parent`.
    pub fn children(&self, parent: NodeId) -> ChildrenIter<'_> {
        ChildrenIter {
            nodes: &self.nodes,
            next: self.nodes[parent.index()].first_child,
        }
    }

    /// Graft an entire `child` arena under `parent`, renaming the grafted
    /// root to `child_name` (if `Some`). Preserves hardlinks, flags, and
    /// per-directory freshness timestamps via a single linear index-shift pass
    /// with zero DFS stack, zero `id_map`, and zero hash-table probing.
    pub fn graft_subtree(
        &mut self,
        parent: NodeId,
        child: &Self,
        child_name: Option<&str>,
    ) -> NodeId {
        let base_node = self.nodes.len();
        let base_name = self.strings.append_raw(&child.strings);
        let root_override = child_name.map(|name| self.strings.push_raw(name));
        let shift = |id: NodeId| {
            NodeId::from_index(id.index() + base_node)
                .expect("arena node count fits in u32")
        };

        self.nodes.reserve(child.nodes.len());
        self.last_child.reserve(child.last_child.len());
        for (i, src) in child.nodes.iter().enumerate() {
            let mut copy = *src;
            copy.name = if i == 0
                && let Some(override_id) = root_override
            {
                override_id
            } else {
                NameId(src.name.0 + base_name)
            };
            copy.parent = if i == 0 {
                Some(parent)
            } else {
                src.parent.map(shift)
            };
            copy.first_child = src.first_child.map(shift);
            copy.next_sibling = if i == 0 {
                None
            } else {
                src.next_sibling.map(shift)
            };
            self.nodes.push(copy);
            self.last_child.push(child.last_child[i].map(shift));
        }

        let grafted_root = NodeId::from_index(base_node)
            .expect("arena node count fits in u32");
        self.link_child(parent, grafted_root);

        self.hardlinks.reserve(child.hardlinks.len());
        for link in &child.hardlinks {
            self.hardlinks.push(HardlinkEntry {
                node: shift(link.node),
                dev: link.dev,
                ino: link.ino,
            });
        }

        grafted_root
    }

    /// Replace the children and metadata of an existing node `target` with
    /// the contents of `replacement`.
    pub fn replace_node_contents(
        &mut self,
        target: NodeId,
        replacement: &Self,
    ) {
        let src_root = replacement.node(replacement.root());
        {
            let dst = &mut self.nodes[target.index()];
            dst.first_child = None;
            dst.allocated_bytes = src_root.allocated_bytes;
            dst.apparent_bytes = src_root.apparent_bytes;
            dst.files = src_root.files;
            dst.dirs = src_root.dirs;
            dst.flags = src_root.flags;
            dst.modified = src_root.modified;
            dst.scanned_at = src_root.scanned_at;
        }
        self.last_child[target.index()] = None;

        let mut id_map: Vec<Option<NodeId>> =
            vec![None; replacement.nodes.len()];
        id_map[replacement.root().index()] = Some(target);

        let mut stack: Vec<(NodeId, NodeId)> = replacement
            .children(replacement.root())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|kid| (kid, target))
            .collect();
        let mut kids = Vec::new();

        while let Some((src_id, dst_parent)) = stack.pop() {
            let src = replacement.node(src_id);
            let name_id = self.strings.intern(replacement.name(src_id));
            let dst_id = NodeId::from_index(self.nodes.len())
                .expect("arena node count fits in u32");
            let mut copy = *src;
            copy.name = name_id;
            copy.parent = Some(dst_parent);
            copy.first_child = None;
            copy.next_sibling = None;
            self.nodes.push(copy);
            self.last_child.push(None);
            self.link_child(dst_parent, dst_id);
            id_map[src_id.index()] = Some(dst_id);

            kids.clear();
            kids.extend(replacement.children(src_id));
            for &kid in kids.iter().rev() {
                stack.push((kid, dst_id));
            }
        }

        for link in &replacement.hardlinks {
            if let Some(mapped) = id_map[link.node.index()] {
                self.hardlinks.push(HardlinkEntry {
                    node: mapped,
                    dev: link.dev,
                    ino: link.ino,
                });
            }
        }
    }

    /// Extract the subtree rooted at `sub_root` into a standalone
    /// [`TreeArena`].
    #[must_use]
    pub fn extract_subtree(&self, sub_root: NodeId) -> Self {
        let sub_path = self.path_of(sub_root);
        let sub_name = self.name(sub_root);
        let mut out = Self::new(sub_path, sub_name);
        let src_root = self.node(sub_root);
        out.nodes[0].allocated_bytes = src_root.allocated_bytes;
        out.nodes[0].apparent_bytes = src_root.apparent_bytes;
        out.nodes[0].files = src_root.files;
        out.nodes[0].dirs = src_root.dirs;
        out.nodes[0].flags = src_root.flags;
        out.nodes[0].modified = src_root.modified;
        out.nodes[0].scanned_at = src_root.scanned_at;

        let mut id_map: Vec<Option<NodeId>> = vec![None; self.nodes.len()];
        id_map[sub_root.index()] = Some(out.root());

        let mut stack: Vec<(NodeId, NodeId)> = self
            .children(sub_root)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|kid| (kid, out.root()))
            .collect();
        let mut kids = Vec::new();

        while let Some((src_id, dst_parent)) = stack.pop() {
            let src = self.node(src_id);
            let name_id = out.strings.intern(self.name(src_id));
            let dst_id = NodeId::from_index(out.nodes.len())
                .expect("arena node count fits in u32");
            let mut copy = *src;
            copy.name = name_id;
            copy.parent = Some(dst_parent);
            copy.first_child = None;
            copy.next_sibling = None;
            out.nodes.push(copy);
            out.last_child.push(None);
            out.link_child(dst_parent, dst_id);
            id_map[src_id.index()] = Some(dst_id);

            kids.clear();
            kids.extend(self.children(src_id));
            for &kid in kids.iter().rev() {
                stack.push((kid, dst_id));
            }
        }

        for link in &self.hardlinks {
            if let Some(mapped) = id_map[link.node.index()] {
                out.hardlinks.push(HardlinkEntry {
                    node: mapped,
                    dev: link.dev,
                    ino: link.ino,
                });
            }
        }

        out
    }

    /// Reconstruct the absolute path of `id` by walking parent pointers.
    pub fn path_of(&self, id: NodeId) -> PathBuf {
        let mut parts: Vec<NameId> = Vec::new();
        let mut current = id;
        while let Some(parent) = self.nodes[current.index()].parent {
            parts.push(self.nodes[current.index()].name);
            current = parent;
        }
        let mut path = self.root_path.clone();
        for name_id in parts.into_iter().rev() {
            path.push(self.strings.get(name_id));
        }
        path
    }

    /// Locate a descendant by relative path components from the arena root.
    pub fn find_relative<'a>(
        &self,
        components: impl IntoIterator<Item = &'a str>,
    ) -> Option<NodeId> {
        let mut current = self.root();
        for component in components {
            let next = self
                .children(current)
                .find(|&child| self.name(child) == component)?;
            current = next;
        }
        Some(current)
    }

    /// Derive every directory's `allocated_bytes`, `apparent_bytes`, `files`,
    /// `dirs`, and `modified` bottom-up from leaf entries, deduplicating
    /// hardlinks if requested, and optionally filtering hidden entries from
    /// totals when `include_hidden` is false.
    pub fn aggregate(&mut self, dedup_hardlinks: bool, include_hidden: bool) {
        let mut order = Vec::with_capacity(self.nodes.len());
        let mut reachable = vec![false; self.nodes.len()];
        let mut stack = vec![self.root()];
        while let Some(id) = stack.pop() {
            reachable[id.index()] = true;
            order.push(id);
            for child in self.children(id) {
                stack.push(child);
            }
        }

        // Mark hardlink duplicates directly in each node's flag bits instead
        // of allocating a second HashSet and probing it for every leaf.
        for link in &self.hardlinks {
            self.nodes[link.node.index()]
                .flags
                .set(NodeFlags::DEDUPED_LINK, false);
        }
        if dedup_hardlinks && !self.hardlinks.is_empty() {
            let mut seen = FxHashSet::with_capacity_and_hasher(
                self.hardlinks.len(),
                rustc_hash::FxBuildHasher,
            );
            for link in &self.hardlinks {
                if !reachable[link.node.index()] {
                    continue;
                }
                if !include_hidden && self.is_under_hidden(link.node) {
                    continue;
                }
                if !seen.insert((link.dev, link.ino)) {
                    self.nodes[link.node.index()]
                        .flags
                        .set(NodeFlags::DEDUPED_LINK, true);
                }
            }
        }

        for &id in order.iter().rev() {
            if !self.nodes[id.index()].is_dir() {
                continue;
            }

            let mut alloc = 0u64;
            let mut apparent = 0u64;
            let mut files = 0u64;
            let mut dirs = 1u32;
            let mut newest = self.nodes[id.index()].modified;

            let mut child_opt = self.nodes[id.index()].first_child;
            while let Some(child_id) = child_opt {
                let child = self.nodes[child_id.index()];
                child_opt = child.next_sibling;
                if !include_hidden && child.hidden() {
                    continue;
                }
                alloc = alloc.saturating_add(child.effective_allocated_bytes());
                apparent =
                    apparent.saturating_add(child.effective_apparent_bytes());
                files = files.saturating_add(child.files);
                dirs = dirs.saturating_add(child.dirs);
                if child.modified > newest {
                    newest = child.modified;
                }
            }

            let node = &mut self.nodes[id.index()];
            node.allocated_bytes = alloc;
            node.apparent_bytes = apparent;
            node.files = files;
            node.dirs = dirs;
            node.modified = newest;
        }
    }

    /// Sort every directory's sibling chain descending by `metric` (tie-break
    /// by name ascending for deterministic output).
    pub fn sort_children(&mut self, metric: Metric) {
        let mut kids = Vec::new();
        for idx in 0..self.nodes.len() {
            if !self.nodes[idx].is_dir() {
                continue;
            }
            let Some(first) = self.nodes[idx].first_child else {
                continue;
            };
            if self.nodes[first.index()].next_sibling.is_none() {
                continue;
            }
            kids.clear();
            let mut cur = Some(first);
            while let Some(kid) = cur {
                kids.push(kid);
                cur = self.nodes[kid.index()].next_sibling;
            }
            kids.sort_unstable_by(|&a, &b| {
                let na = &self.nodes[a.index()];
                let nb = &self.nodes[b.index()];
                nb.value(metric).cmp(&na.value(metric)).then_with(|| {
                    self.strings.get(na.name).cmp(self.strings.get(nb.name))
                })
            });
            self.nodes[idx].first_child = kids.first().copied();
            self.last_child[idx] = kids.last().copied();
            for pair in kids.windows(2) {
                self.nodes[pair[0].index()].next_sibling = Some(pair[1]);
            }
            if let Some(&last) = kids.last() {
                self.nodes[last.index()].next_sibling = None;
            }
        }
    }

    /// Apply a status tag to every directory in the arena based on `now` and
    /// `ttl_secs`.
    pub fn refresh_statuses(&mut self, now: u64, ttl_secs: u64) {
        for node in &mut self.nodes {
            if !node.is_dir() {
                continue;
            }
            if node.scanned_at == 0 {
                node.set_status(SubtreeStatus::Unscanned);
            } else if now.saturating_sub(node.scanned_at) <= ttl_secs {
                node.set_status(SubtreeStatus::Fresh {
                    scanned_at: node.scanned_at,
                });
            } else {
                node.set_status(SubtreeStatus::Stale {
                    scanned_at: node.scanned_at,
                });
            }
        }
    }

    /// Count how many nodes live at or beneath every node in the arena.
    #[cfg(feature = "cache-db")]
    pub(crate) fn subtree_counts(&self) -> Vec<u32> {
        let mut order = Vec::with_capacity(self.nodes.len());
        let mut stack = vec![self.root()];
        while let Some(id) = stack.pop() {
            order.push(id);
            for child in self.children(id) {
                stack.push(child);
            }
        }
        let mut counts = vec![1u32; self.nodes.len()];
        for &id in order.iter().rev() {
            if let Some(parent) = self.nodes[id.index()].parent {
                counts[parent.index()] =
                    counts[parent.index()].saturating_add(counts[id.index()]);
            }
        }
        counts
    }

    /// Direct leaf bytes inside `dir` (or this file's own bytes).
    pub fn own_bytes(&self, id: NodeId, apparent: bool) -> u64 {
        let node = self.node(id);
        if !node.is_dir() {
            return if apparent {
                node.effective_apparent_bytes()
            } else {
                node.effective_allocated_bytes()
            };
        }
        self.children(id)
            .filter(|&child| !self.node(child).is_dir())
            .fold(0u64, |sum, child| {
                let c = self.node(child);
                let val = if apparent {
                    c.effective_apparent_bytes()
                } else {
                    c.effective_allocated_bytes()
                };
                sum.saturating_add(val)
            })
    }

    fn is_under_hidden(&self, mut id: NodeId) -> bool {
        loop {
            let node = self.nodes[id.index()];
            if node.parent.is_none() {
                return false;
            }
            if node.hidden() {
                return true;
            }
            let Some(parent) = node.parent else {
                return false;
            };
            id = parent;
        }
    }

    /// Canonicalized root path for this arena.
    pub fn root_path(&self) -> &Path {
        &self.root_path
    }
}

/// Zero-allocation iterator over a directory's direct children.
#[derive(Clone, Debug)]
pub struct ChildrenIter<'a> {
    nodes: &'a [ArenaNode],
    next: Option<NodeId>,
}

impl Iterator for ChildrenIter<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        let current = self.next?;
        self.next = self.nodes[current.index()].next_sibling;
        Some(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_node_stays_compact() {
        // Keep each node to a single 64-byte cache line.
        assert_eq!(std::mem::size_of::<ArenaNode>(), 64);
    }

    #[test]
    fn aggregate_and_dedup_hardlinks_across_grafted_subtrees() {
        let mut sub = TreeArena::new(PathBuf::from("/root/sub"), "sub");
        let f1 = sub.add_child(
            sub.root(),
            "shared.bin",
            NodeKind::File,
            4096,
            100,
            10,
            NodeFlags::new(NodeKind::File),
        );
        sub.record_hardlink(f1, 1, 999);
        sub.aggregate(true, true);
        assert_eq!(sub.node(sub.root()).allocated_bytes, 4096);

        let mut root = TreeArena::new(PathBuf::from("/root"), "root");
        let f2 = root.add_child(
            root.root(),
            "alias.bin",
            NodeKind::File,
            4096,
            100,
            20,
            NodeFlags::new(NodeKind::File),
        );
        root.record_hardlink(f2, 1, 999);
        root.graft_subtree(root.root(), &sub, None);

        root.aggregate(true, true);
        assert_eq!(root.node(root.root()).allocated_bytes, 4096);
        assert_eq!(root.node(root.root()).apparent_bytes, 100);
        assert_eq!(root.node(root.root()).files, 2);
        assert_eq!(root.node(root.root()).dirs, 2);
    }

    #[test]
    fn extract_and_path_reconstruction_work() {
        let mut arena = TreeArena::new(PathBuf::from("/home/tobi"), "tobi");
        let src = arena.add_child(
            arena.root(),
            "src",
            NodeKind::Directory,
            0,
            0,
            0,
            NodeFlags::new(NodeKind::Directory),
        );
        let file = arena.add_child(
            src,
            "main.rs",
            NodeKind::File,
            4096,
            250,
            123,
            NodeFlags::new(NodeKind::File),
        );
        assert_eq!(
            arena.path_of(file),
            PathBuf::from("/home/tobi/src/main.rs")
        );

        let extracted = arena.extract_subtree(src);
        assert_eq!(extracted.root_path(), Path::new("/home/tobi/src"));
        let found = extracted.find_relative(["main.rs"]).expect("found");
        assert_eq!(extracted.node(found).apparent_bytes, 250);
    }
}

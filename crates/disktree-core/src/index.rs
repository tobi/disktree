//! How a scanned tree is held: in a handful of buffers, by `u32` index.
//!
//! Every node is a 64-byte record in one vector, each directory's children
//! a contiguous run of it, every name in one string, and the allocation
//! and sharing records, which most nodes lack, in side tables.
//!
//! A tree of separately allocated nodes is freed one `free()` at a time,
//! each write reading a page back in, from the compressor if macOS has
//! packed it away: 0.8 to 0.9 s for a 20-million-node tree in a fresh
//! process, 15 s in a window under memory pressure. Here a drop frees five
//! buffers without reading them, in under a millisecond and with no page
//! faults. Nothing here needs `unsafe`: an index is checked where a pointer
//! would have to be trusted, and a name is a slice of a `String`.
//!
//! The walk builds on many workers at once. Each appends a directory's
//! children as one block when the directory completes, under one lock, so
//! blocks land in whatever order workers finish. [`Tree::build`] then
//! reorders the nodes once so that every subtree is one contiguous run,
//! and from then on a pass can hand separate subtrees to separate workers
//! by splitting the vector, as [`Tree::par_bottom_up`] does. On a tree of
//! 19.9 million nodes that took 2.6 to 3.9 s, and it is what keeps the
//! totals pass at 0.1 to 0.2 s instead of 1.2 to 1.8 s in build order.
//!
//! Reading goes through [`Node`], a cursor that pairs a stored record with
//! the tree it indexes into; writing goes through [`NodeMut`], or a pass
//! over the whole tree.

use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroU64;
use std::ops::Deref;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rayon::prelude::*;

use crate::classify::{Category, Reclaim};
use crate::sharing::{Allocation, Sharing};
use crate::tree::{Draft, Metric, NodeKind};

/// The longest name a tree holds.
///
/// A local file system allows a few hundred bytes, even decoded lossily;
/// anything read from elsewhere, such as an SSH host's stream, is refused
/// past this before it is built.
pub const MAX_NAME_BYTES: usize = 4096;

/// Everything about a node that is plain data, and so free to write.
///
/// `bytes` and `files` are the subtree totals the treemap draws. A disk
/// holds tens of millions of nodes, so every field is paid that many
/// times: the size of a stored node is held by a test.
#[derive(Clone, Copy, Debug)]
pub struct Facts {
    pub kind: NodeKind,
    /// Subtree total: direct contents plus every descendant. A file's own
    /// size, or nothing for a second name of a file already charged.
    pub bytes: u64,
    /// Files at or beneath this node; `1` for a file.
    pub files: u64,
    /// Directories at or beneath this node; `1` for a directory. Four
    /// billion is more than any volume holds.
    pub dirs: u32,
    /// `(device, inode)` for files, used to de-duplicate hardlinks. No file
    /// has inode zero, which lets `None` cost nothing.
    pub inode: Option<(u64, NonZeroU64)>,
    /// Some entries here or below could not be read; totals are incomplete.
    pub read_error: bool,
    /// Newest write time at or beneath this node, in Unix seconds; `0` when
    /// unknown. Derived for directories by [`crate::tree::aggregate`].
    pub modified: i64,
    /// What kind of data this is, for colour. Set by [`crate::classify`].
    pub category: Category,
    /// Why this space can be had back, if it can. Set by
    /// [`crate::classify`]; inherited by everything beneath.
    pub reclaim: Option<Reclaim>,
}

impl Facts {
    /// A directory with nothing counted yet.
    pub const fn directory() -> Self {
        Self {
            kind: NodeKind::Directory,
            bytes: 0,
            files: 0,
            dirs: 1,
            inode: None,
            read_error: false,
            modified: 0,
            category: Category::Other,
            reclaim: None,
        }
    }

    /// A leaf entry of `bytes`.
    pub fn entry(kind: NodeKind, bytes: u64) -> Self {
        Self {
            kind,
            bytes,
            files: u64::from(kind == NodeKind::File),
            dirs: 0,
            ..Self::directory()
        }
    }

    /// The value a treemap should weight this node by.
    pub const fn value(&self, metric: Metric) -> u64 {
        match metric {
            Metric::Bytes => self.bytes,
            Metric::Files => self.files,
        }
    }
}

/// Which side table a node's [`Extra`] points into, in its top two bits.
///
/// A node carries one index instead of one per table: an allocation is
/// kept for few files and sharing figures for some directories, so two
/// indices would cost every node four bytes for what most never use, and
/// take a stored node from 64 bytes to 72.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Extra(u32);

impl Extra {
    const NONE: Self = Self(0);
    const SHIFT: u32 = 30;
    const ALLOCATION: u32 = 1;
    const SHARING: u32 = 2;
    const BOTH: u32 = 3;

    fn new(tag: u32, index: usize) -> Self {
        let index = u32::try_from(index)
            .ok()
            .filter(|index| *index < 1 << Self::SHIFT)
            .expect("fewer than a billion side records");
        Self(tag << Self::SHIFT | index)
    }

    const fn tag(self) -> u32 {
        self.0 >> Self::SHIFT
    }

    const fn index(self) -> usize {
        (self.0 & ((1 << Self::SHIFT) - 1)) as usize
    }
}

/// One node as stored: its facts and where its name, children and side
/// records are.
#[derive(Clone, Copy, Debug)]
struct Slot {
    facts: Facts,
    /// Offset of the name's length prefix in [`Store::names`].
    name: u32,
    /// Index of the first child in [`Store::nodes`]; zero when there are
    /// none.
    first: u32,
    len: u32,
    extra: Extra,
}

/// Everything a tree owns.
#[derive(Default)]
struct Store {
    nodes: Vec<Slot>,
    /// Every name, each after its length written as one `char`: a `String`
    /// can be sliced without checking its bytes again, which `&[u8]` could
    /// only skip with `unsafe`, and a length that is a `char` keeps the
    /// buffer valid UTF-8. A name is at most a few hundred bytes, well
    /// under the surrogate range a `char` cannot hold.
    names: String,
    allocations: Vec<Allocation>,
    sharings: Vec<Sharing>,
    /// A node with both, which only a followed directory link has.
    both: Vec<(Allocation, Sharing)>,
    root: u32,
}

/// A copy made on every worker: copying a large tree is gigabytes of
/// writes to pages touched for the first time, which one thread would
/// fault in one at a time.
impl Clone for Store {
    fn clone(&self) -> Self {
        fn copied<T: Copy + Send + Sync>(source: &[T]) -> Vec<T> {
            let mut copy = Vec::with_capacity(source.len());
            copy.par_extend(source.par_iter().copied());
            copy
        }
        let ((nodes, allocations), (names, sharings)) = rayon::join(
            || {
                rayon::join(
                    || copied(&self.nodes),
                    || copied(&self.allocations),
                )
            },
            || rayon::join(|| self.names.clone(), || copied(&self.sharings)),
        );
        Self {
            nodes,
            names,
            allocations,
            sharings,
            both: self.both.clone(),
            root: self.root,
        }
    }
}

fn name_at(names: &str, at: u32) -> &str {
    let rest = &names[at as usize..];
    let mut chars = rest.chars();
    let len = chars.next().map_or(0, |len| len as usize);
    let start = rest.len() - chars.as_str().len();
    &rest[start..start + len]
}

impl Store {
    fn index(len: usize) -> u32 {
        u32::try_from(len).expect("fewer than four billion nodes")
    }

    fn name(&self, at: u32) -> &str {
        name_at(&self.names, at)
    }

    fn push_name(&mut self, name: &str) -> u32 {
        let at = u32::try_from(self.names.len())
            .expect("a tree's names fit in four gigabytes");
        // `MAX_NAME_BYTES` is far below the first length that is not a
        // `char`, 55,296, where the surrogates begin.
        assert!(name.len() <= MAX_NAME_BYTES, "a name of {}", name.len());
        let len = u32::try_from(name.len())
            .ok()
            .and_then(char::from_u32)
            .expect("a length below the surrogates");
        self.names.push(len);
        self.names.push_str(name);
        at
    }

    fn allocation(&self, extra: Extra) -> Option<&Allocation> {
        match extra.tag() {
            Extra::ALLOCATION => Some(&self.allocations[extra.index()]),
            Extra::BOTH => Some(&self.both[extra.index()].0),
            _ => None,
        }
    }

    fn sharing(&self, extra: Extra) -> Option<&Sharing> {
        match extra.tag() {
            Extra::SHARING => Some(&self.sharings[extra.index()]),
            Extra::BOTH => Some(&self.both[extra.index()].1),
            _ => None,
        }
    }

    fn extra(
        &mut self,
        allocation: Option<Allocation>,
        sharing: Option<Sharing>,
    ) -> Extra {
        match (allocation, sharing) {
            (None, None) => Extra::NONE,
            (Some(allocation), None) => {
                self.allocations.push(allocation);
                Extra::new(Extra::ALLOCATION, self.allocations.len() - 1)
            }
            (None, Some(sharing)) => {
                self.sharings.push(sharing);
                Extra::new(Extra::SHARING, self.sharings.len() - 1)
            }
            (Some(allocation), Some(sharing)) => {
                self.both.push((allocation, sharing));
                Extra::new(Extra::BOTH, self.both.len() - 1)
            }
        }
    }

    /// Replace a node's sharing figures, in place when it has some, so
    /// aggregating again does not grow the tables. A record left behind
    /// when a node changes table stays until the tree is dropped; only
    /// a change in what is kept does that, which re-ranking never makes.
    fn set_sharing(&mut self, extra: Extra, sharing: Option<Sharing>) -> Extra {
        let index = extra.index();
        match (extra.tag(), sharing) {
            (Extra::SHARING, Some(value)) => {
                self.sharings[index] = value;
                extra
            }
            (Extra::BOTH, Some(value)) => {
                self.both[index].1 = value;
                extra
            }
            (Extra::ALLOCATION, Some(value)) => {
                let allocation = self.allocations[index];
                self.extra(Some(allocation), Some(value))
            }
            (Extra::BOTH, None) => {
                let allocation = self.both[index].0;
                self.extra(Some(allocation), None)
            }
            (Extra::SHARING, None) => Extra::NONE,
            (_, None) => extra,
            (_, value) => self.extra(None, value),
        }
    }

    /// A node for `name` with these facts and records, holding `children`,
    /// which become one block of this store.
    fn place(
        &mut self,
        name: &str,
        facts: Facts,
        allocation: Option<Allocation>,
        sharing: Option<Sharing>,
        children: impl ExactSizeIterator<Item = Slot>,
    ) -> Slot {
        let len = Self::index(children.len());
        let first = if len == 0 {
            0
        } else {
            let first = Self::index(self.nodes.len());
            self.nodes.extend(children);
            first
        };
        Slot {
            facts,
            name: self.push_name(name),
            first,
            len,
            extra: self.extra(allocation, sharing),
        }
    }

    fn children(&self, slot: &Slot) -> &[Slot] {
        let first = slot.first as usize;
        &self.nodes[first..first + slot.len as usize]
    }

    /// A copy of `node` from any tree, and everything beneath it.
    fn copy(&mut self, node: Node<'_>, name: &str) -> Slot {
        let children: Vec<Slot> = node
            .children()
            .iter()
            .map(|child| self.copy(child, child.name()))
            .collect();
        self.place(
            name,
            *node.facts(),
            node.allocation().copied(),
            node.stored_sharing().copied(),
            children.into_iter(),
        )
    }

    /// A copy of all of `source` under `name`: its buffers appended in
    /// order, each index moved past what this store already holds. One
    /// sequential pass instead of a walk with a list per directory, which
    /// matters because a build copies under the lock every worker needs.
    /// `source`'s own root is appended too and left for
    /// [`Self::relayout`] to cut, which keeps every index a plain shift.
    fn absorb(&mut self, source: &Self, name: &str) -> Slot {
        let base = Self::index(self.nodes.len());
        let names = Self::index(self.names.len());
        let allocations = self.allocations.len();
        let sharings = self.sharings.len();
        let both = self.both.len();
        self.names.push_str(&source.names);
        u32::try_from(self.names.len())
            .expect("a tree's names fit in four gigabytes");
        self.allocations.extend_from_slice(&source.allocations);
        self.sharings.extend_from_slice(&source.sharings);
        self.both.extend_from_slice(&source.both);
        let moved = |slot: &Slot| {
            let index = slot.extra.index();
            let extra = match slot.extra.tag() {
                Extra::ALLOCATION => {
                    Extra::new(Extra::ALLOCATION, allocations + index)
                }
                Extra::SHARING => Extra::new(Extra::SHARING, sharings + index),
                Extra::BOTH => Extra::new(Extra::BOTH, both + index),
                _ => Extra::NONE,
            };
            Slot {
                name: names + slot.name,
                first: if slot.len == 0 { 0 } else { base + slot.first },
                extra,
                ..*slot
            }
        };
        // Not in parallel: this runs under the builder's lock, and a rayon
        // worker waiting on its own jobs can pick up a walk task that asks
        // for the same lock.
        self.nodes.extend(source.nodes.iter().map(moved));
        Slot {
            name: self.push_name(name),
            ..moved(&source.nodes[source.root as usize])
        }
    }

    fn draft(&mut self, draft: &Draft) -> Slot {
        let children: Vec<Slot> = draft
            .children
            .iter()
            .map(|child| self.draft(child))
            .collect();
        self.place(
            &draft.name,
            draft.facts,
            draft.allocation.as_deref().copied(),
            draft.sharing.as_deref().copied(),
            children.into_iter(),
        )
    }

    /// Reorder the nodes so that every subtree is one contiguous run: the
    /// root at index 0, then each directory's block followed by each of its
    /// children's runs, in block order. That is what lets a pass hand
    /// separate subtrees to separate workers with `split_at_mut`, where a
    /// build's order, interleaved by whichever worker finished first,
    /// gives no such split. Sorting a block later keeps it true: a child
    /// moves within its block and its run stays where it is.
    ///
    /// In place, by a permutation of four bytes a node, instead of a second
    /// copy of the nodes at sixty-four. Nodes no directory reaches are
    /// dropped.
    fn relayout(&mut self) {
        let len = self.nodes.len();
        let mut target = vec![u32::MAX; len];
        target[self.root as usize] = 0;
        let mut next: u32 = 1;
        // Depth first: a directory's block, then each child's run.
        let mut stack: Vec<(u32, u32)> = Vec::new();
        let mut place =
            |slot: &Slot, target: &mut [u32], stack: &mut Vec<(u32, u32)>| {
                if slot.len > 0 {
                    for offset in 0..slot.len {
                        target[(slot.first + offset) as usize] = next + offset;
                    }
                    next += slot.len;
                    stack.push((slot.first, slot.first + slot.len));
                }
            };
        place(&self.nodes[self.root as usize], &mut target, &mut stack);
        while let Some((child, end)) = stack.pop() {
            if child + 1 < end {
                stack.push((child + 1, end));
            }
            place(&self.nodes[child as usize], &mut target, &mut stack);
        }
        let reached = next;
        // What nothing reaches goes past the end, to be cut off.
        for slot in &mut target {
            if *slot == u32::MAX {
                *slot = next;
                next += 1;
            }
        }
        // A block moves whole, so its new start is its first child's.
        for slot in &mut self.nodes {
            if slot.len > 0 {
                slot.first = target[slot.first as usize];
            }
        }
        for index in 0..len {
            while target[index] as usize != index {
                let to = target[index] as usize;
                self.nodes.swap(index, to);
                target.swap(index, to);
            }
        }
        self.nodes.truncate(reached as usize);
        self.root = 0;
    }

    const fn bytes(&self) -> StorageBytes {
        StorageBytes {
            used: self.nodes.len() * size_of::<Slot>()
                + self.names.len()
                + self.allocations.len() * size_of::<Allocation>()
                + self.sharings.len() * size_of::<Sharing>()
                + self.both.len() * size_of::<(Allocation, Sharing)>(),
            reserved: self.nodes.capacity() * size_of::<Slot>()
                + self.names.capacity()
                + self.allocations.capacity() * size_of::<Allocation>()
                + self.sharings.capacity() * size_of::<Sharing>()
                + self.both.capacity() * size_of::<(Allocation, Sharing)>(),
        }
    }
}

/// What a tree's buffers hold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StorageBytes {
    /// Bytes holding nodes, names and side records.
    pub used: usize,
    /// Bytes the buffers have room for, including growth room that is
    /// never touched and so never resident.
    pub reserved: usize,
}

/// One entry in a scanned tree, read through the tree that holds it.
/// Its facts read through `Deref`.
#[derive(Clone, Copy)]
pub struct Node<'t> {
    store: &'t Store,
    slot: &'t Slot,
}

impl Deref for Node<'_> {
    type Target = Facts;

    fn deref(&self) -> &Facts {
        &self.slot.facts
    }
}

impl<'t> Node<'t> {
    pub const fn facts(self) -> &'t Facts {
        &self.slot.facts
    }

    pub fn name(self) -> &'t str {
        self.store.name(self.slot.name)
    }

    /// Children, ordered by [`crate::tree::Metric`] value, descending.
    pub fn children(self) -> Children<'t> {
        Children {
            store: self.store,
            slots: self.store.children(self.slot),
        }
    }

    pub fn allocation(self) -> Option<&'t Allocation> {
        self.store.allocation(self.slot.extra)
    }

    /// Derived clone figures, sparse: see [`Node::sharing`].
    pub fn stored_sharing(self) -> Option<&'t Sharing> {
        self.store.sharing(self.slot.extra)
    }

    /// Where this node is in its tree, for [`Tree::set_sharing`]: a pass
    /// that only reads can work out figures on every worker, then write
    /// them by position. Good until the tree is next changed.
    pub(crate) fn id(self) -> u32 {
        debug_assert!(
            self.store
                .nodes
                .as_ptr_range()
                .contains(&std::ptr::from_ref(self.slot)),
            "a node of another tree"
        );
        let offset = std::ptr::from_ref(self.slot).addr()
            - self.store.nodes.as_ptr().addr();
        Store::index(offset / size_of::<Slot>())
    }

    /// Whether this is its tree's root, and so the whole tree.
    fn is_root(self) -> bool {
        self.id() == self.store.root
    }
}

impl fmt::Debug for Node<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("name", &self.name())
            .field("facts", self.facts())
            .field("allocation", &self.allocation())
            .field("sharing", &self.stored_sharing())
            .field("children", &self.children())
            .finish()
    }
}

/// A directory's children: a contiguous run of its tree's nodes.
#[derive(Clone, Copy)]
pub struct Children<'t> {
    store: &'t Store,
    slots: &'t [Slot],
}

impl<'t> Children<'t> {
    pub const fn len(self) -> usize {
        self.slots.len()
    }

    pub const fn is_empty(self) -> bool {
        self.slots.is_empty()
    }

    pub fn get(self, index: usize) -> Option<Node<'t>> {
        let store = self.store;
        self.slots.get(index).map(|slot| Node { store, slot })
    }

    /// The child at `index`, which must exist.
    pub fn at(self, index: usize) -> Node<'t> {
        Node {
            store: self.store,
            slot: &self.slots[index],
        }
    }

    pub fn iter(self) -> Iter<'t> {
        Iter {
            store: self.store,
            slots: self.slots.iter(),
        }
    }

    pub fn par_iter(self) -> impl IndexedParallelIterator<Item = Node<'t>> {
        let store = self.store;
        self.slots.par_iter().map(move |slot| Node { store, slot })
    }
}

impl<'t> IntoIterator for Children<'t> {
    type Item = Node<'t>;
    type IntoIter = Iter<'t>;

    fn into_iter(self) -> Iter<'t> {
        self.iter()
    }
}

impl fmt::Debug for Children<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// The children of one directory, in order.
#[derive(Clone)]
pub struct Iter<'t> {
    store: &'t Store,
    slots: std::slice::Iter<'t, Slot>,
}

impl fmt::Debug for Iter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Iter")
            .field("remaining", &self.slots.len())
            .finish()
    }
}

impl<'t> Iterator for Iter<'t> {
    type Item = Node<'t>;

    fn next(&mut self) -> Option<Node<'t>> {
        let store = self.store;
        self.slots.next().map(|slot| Node { store, slot })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.slots.size_hint()
    }

    fn nth(&mut self, n: usize) -> Option<Node<'t>> {
        let store = self.store;
        self.slots.nth(n).map(|slot| Node { store, slot })
    }
}

impl DoubleEndedIterator for Iter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        let store = self.store;
        self.slots.next_back().map(|slot| Node { store, slot })
    }
}

impl ExactSizeIterator for Iter<'_> {}

impl std::iter::FusedIterator for Iter<'_> {}

/// What a sort of one directory's children compares.
#[derive(Clone, Copy, Debug)]
pub struct Sibling<'a> {
    facts: &'a Facts,
    pub name: &'a str,
}

impl Deref for Sibling<'_> {
    type Target = Facts;

    fn deref(&self) -> &Facts {
        self.facts
    }
}

/// Write access to one node of a tree, and through it to its subtree.
#[derive(Debug)]
pub struct NodeMut<'t> {
    store: &'t mut Store,
    id: u32,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("nodes", &self.nodes.len())
            .field("names", &self.names.len())
            .finish_non_exhaustive()
    }
}

impl Deref for NodeMut<'_> {
    type Target = Facts;

    fn deref(&self) -> &Facts {
        &self.store.nodes[self.id as usize].facts
    }
}

impl NodeMut<'_> {
    /// This node, to read.
    pub fn node(&self) -> Node<'_> {
        Node {
            store: self.store,
            slot: &self.store.nodes[self.id as usize],
        }
    }

    pub fn facts_mut(&mut self) -> &mut Facts {
        &mut self.store.nodes[self.id as usize].facts
    }

    /// This node again, for a shorter borrow.
    pub const fn reborrow(&mut self) -> NodeMut<'_> {
        NodeMut {
            store: self.store,
            id: self.id,
        }
    }

    pub fn child_count(&self) -> usize {
        self.store.nodes[self.id as usize].len as usize
    }

    pub fn child_mut(&mut self, index: usize) -> Option<NodeMut<'_>> {
        let slot = &self.store.nodes[self.id as usize];
        let index = u32::try_from(index).ok().filter(|i| *i < slot.len)?;
        Some(NodeMut {
            id: slot.first + index,
            store: self.store,
        })
    }

    /// Visit each child in order, one at a time.
    pub fn for_each_child_mut(&mut self, mut visit: impl FnMut(NodeMut<'_>)) {
        let slot = self.store.nodes[self.id as usize];
        for id in slot.first..slot.first + slot.len {
            visit(NodeMut {
                store: self.store,
                id,
            });
        }
    }

    /// Reorder the children in place. Each child's own children stay
    /// where they are: a child records where its block is, and moves
    /// with that record.
    pub fn sort_children_by(
        &mut self,
        mut compare: impl FnMut(Sibling<'_>, Sibling<'_>) -> std::cmp::Ordering,
    ) {
        let Store { nodes, names, .. } = &mut *self.store;
        let slot = nodes[self.id as usize];
        let first = slot.first as usize;
        nodes[first..first + slot.len as usize].sort_unstable_by(
            |left, right| {
                compare(
                    Sibling {
                        facts: &left.facts,
                        name: name_at(names, left.name),
                    },
                    Sibling {
                        facts: &right.facts,
                        name: name_at(names, right.name),
                    },
                )
            },
        );
    }

    pub fn set_sharing(&mut self, sharing: Option<Sharing>) {
        let extra = self.store.nodes[self.id as usize].extra;
        let extra = self.store.set_sharing(extra, sharing);
        self.store.nodes[self.id as usize].extra = extra;
    }
}

/// One directory's children, as a whole-tree pass sees them: their facts
/// and names to read, and their facts and order to change.
#[derive(Debug)]
pub struct Block<'a> {
    slots: &'a mut [Slot],
    names: &'a str,
    /// Where the block starts in the tree's nodes.
    first: u32,
}

impl Block<'_> {
    pub const fn len(&self) -> usize {
        self.slots.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The child at `index`, which must exist.
    pub fn at(&self, index: usize) -> Sibling<'_> {
        let slot = &self.slots[index];
        Sibling {
            facts: &slot.facts,
            name: name_at(self.names, slot.name),
        }
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = Sibling<'_>> {
        self.slots.iter().map(|slot| Sibling {
            facts: &slot.facts,
            name: name_at(self.names, slot.name),
        })
    }

    /// The facts of the child at `index`, which must exist.
    pub fn facts_mut(&mut self, index: usize) -> &mut Facts {
        &mut self.slots[index].facts
    }

    /// Every child's facts, in order.
    pub fn for_each_facts_mut(&mut self, visit: impl FnMut(&mut Facts)) {
        self.slots
            .iter_mut()
            .map(|slot| &mut slot.facts)
            .for_each(visit);
    }

    /// The [`Node::id`] of the child at `index`: what a pass that read the
    /// tree first found out about it can be looked up by.
    pub(crate) fn id(&self, index: usize) -> u32 {
        debug_assert!(index < self.slots.len(), "a child of this block");
        self.first + Store::index(index)
    }

    /// Reorder the children; see [`NodeMut::sort_children_by`].
    pub fn sort_by(
        &mut self,
        mut compare: impl FnMut(Sibling<'_>, Sibling<'_>) -> std::cmp::Ordering,
    ) {
        let names = self.names;
        self.slots.sort_unstable_by(|left, right| {
            compare(
                Sibling {
                    facts: &left.facts,
                    name: name_at(names, left.name),
                },
                Sibling {
                    facts: &right.facts,
                    name: name_at(names, right.name),
                },
            )
        });
    }
}

/// `node`, whose descendants are all in `region`, which starts at index
/// `base` of the tree's nodes. In the layout [`Store::relayout`] makes,
/// a directory's block opens its run, and each child's run is the part
/// of what follows that starts at the child's first child.
fn bottom_up<L, D>(
    node: &mut Slot,
    region: &mut [Slot],
    base: usize,
    names: &str,
    depth: usize,
    visit: &(L, D),
) where
    L: Fn(&mut Facts) + Sync,
    D: Fn(&mut Facts, Block<'_>) + Sync,
{
    let (leaf, directory) = visit;
    if !node.facts.kind.is_dir() {
        leaf(&mut node.facts);
        return;
    }
    let len = node.len as usize;
    let start = if len == 0 {
        0
    } else {
        node.first as usize - base
    };
    let (block, rest) = region[start..].split_at_mut(len);
    let rest_base = base + start + len;
    if depth < crate::tree::PARALLEL_LEVELS && len > 1 {
        let parts = runs(block, rest, rest_base);
        block
            .par_iter_mut()
            .zip(parts)
            .for_each(|(child, (run, base))| {
                bottom_up(child, run, base, names, depth + 1, visit);
            });
    } else {
        for child in block.iter_mut() {
            bottom_up(child, rest, rest_base, names, depth + 1, visit);
        }
    }
    let first = node.first;
    directory(
        &mut node.facts,
        Block {
            slots: block,
            names,
            first,
        },
    );
}

/// Beneath `block`, whose facts are written: each child directory's own
/// block with `directory`, then beneath that. `rest` holds the runs of
/// all of `block`'s children and starts at index `base`, as in
/// [`bottom_up`].
fn top_down<D>(
    block: &[Slot],
    rest: &mut [Slot],
    base: usize,
    names: &str,
    depth: usize,
    directory: &D,
) where
    D: Fn(&Facts, Block<'_>) + Sync,
{
    // As in `bottom_up`, a node that is not a directory is a leaf, and a
    // directory that holds nothing has no block to visit.
    let below = |child: &Slot, region: &mut [Slot], base: usize| {
        let len = child.len as usize;
        if !child.facts.kind.is_dir() || len == 0 {
            return;
        }
        let start = child.first as usize - base;
        let (block, rest) = region[start..].split_at_mut(len);
        directory(
            &child.facts,
            Block {
                slots: &mut *block,
                names,
                first: child.first,
            },
        );
        top_down(block, rest, base + start + len, names, depth + 1, directory);
    };
    if depth < crate::tree::PARALLEL_LEVELS && block.len() > 1 {
        block
            .par_iter()
            .zip(runs(block, rest, base))
            .for_each(|(child, (run, base))| below(child, run, base));
    } else {
        for child in block {
            below(child, rest, base);
        }
    }
}

/// Each child's run of `rest`, which holds the runs of all of `block`'s
/// children and starts at index `base`, with the index it starts at; an
/// empty run for a child with no children. Runs are in the order they
/// were laid out, which sorting the block since has not changed.
fn runs<'r>(
    block: &[Slot],
    rest: &'r mut [Slot],
    base: usize,
) -> Vec<(&'r mut [Slot], usize)> {
    let len = block.len();
    let mut runs: Vec<usize> =
        (0..len).filter(|&index| block[index].len > 0).collect();
    runs.sort_unstable_by_key(|&index| block[index].first);
    let mut parts: Vec<(&mut [Slot], usize)> =
        (0..len).map(|_| (&mut [][..], base)).collect();
    let mut rest = rest;
    let mut at = base;
    for (order, &index) in runs.iter().enumerate() {
        let begin = block[index].first as usize;
        let end = runs
            .get(order + 1)
            .map_or(at + rest.len(), |&next| block[next].first as usize);
        let (_, tail) = std::mem::take(&mut rest).split_at_mut(begin - at);
        let (run, tail) = tail.split_at_mut(end - begin);
        parts[index] = (run, begin);
        rest = tail;
        at = end;
    }
    parts
}

/// Where a tree is built: nodes are appended under a lock, so any number
/// of workers can build at once. `'h` brands what it builds with the
/// build it belongs to.
#[derive(Debug, Default)]
pub struct Builder {
    store: Mutex<Store>,
}

/// A node one build has made, not yet placed in its tree.
#[must_use]
pub struct Built<'h> {
    slot: Slot,
    /// Invariant: a node of one build cannot be taken for another's.
    brand: PhantomData<fn(&'h ()) -> &'h ()>,
}

impl fmt::Debug for Built<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.slot.fmt(f)
    }
}

impl Deref for Built<'_> {
    type Target = Facts;

    fn deref(&self) -> &Facts {
        &self.slot.facts
    }
}

impl Built<'_> {
    pub const fn facts_mut(&mut self) -> &mut Facts {
        &mut self.slot.facts
    }
}

impl Builder {
    fn lock(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    const fn built<'h>(slot: Slot) -> Built<'h> {
        Built {
            slot,
            brand: PhantomData,
        }
    }

    /// A leaf, or a directory that holds nothing.
    pub fn leaf<'h>(
        &'h self,
        name: &str,
        facts: Facts,
        allocation: Option<Allocation>,
    ) -> Built<'h> {
        let slot = self.lock().place(
            name,
            facts,
            allocation,
            None,
            std::iter::empty(),
        );
        Self::built(slot)
    }

    /// Several leaves under one lock: how a listing adds its files.
    pub fn leaves<'h, 'a>(
        &'h self,
        leaves: impl IntoIterator<Item = (&'a str, Facts, Option<Allocation>)>,
    ) -> Vec<Built<'h>> {
        let mut store = self.lock();
        leaves
            .into_iter()
            .map(|(name, facts, allocation)| {
                Self::built(store.place(
                    name,
                    facts,
                    allocation,
                    None,
                    std::iter::empty(),
                ))
            })
            .collect()
    }

    /// A directory holding `children`, which become one block of the tree
    /// in the order given.
    pub fn directory<'h>(
        &'h self,
        name: &str,
        facts: Facts,
        allocation: Option<Allocation>,
        children: Vec<Built<'h>>,
    ) -> Built<'h> {
        let slot = self.lock().place(
            name,
            facts,
            allocation,
            None,
            children.into_iter().map(|child| child.slot),
        );
        Self::built(slot)
    }

    /// A deep copy of `node`, from any tree, into this build: how a
    /// subtree moves between trees.
    pub fn copy<'h>(&'h self, node: Node<'_>) -> Built<'h> {
        self.copy_named(node, node.name())
    }

    /// [`Self::copy`], under another name.
    pub fn copy_named<'h>(&'h self, node: Node<'_>, name: &str) -> Built<'h> {
        let mut store = self.lock();
        // A whole tree, as a wider scan reuses one, goes in as its buffers.
        let slot = if node.is_root() {
            store.absorb(node.store, name)
        } else {
            store.copy(node, name)
        };
        Self::built(slot)
    }

    /// `draft`, with everything beneath it.
    pub fn draft<'h>(&'h self, draft: &Draft) -> Built<'h> {
        Self::built(self.lock().draft(draft))
    }
}

/// A scanned tree and the buffers its nodes live in.
#[derive(Clone)]
pub struct Tree {
    store: Store,
}

impl fmt::Debug for Tree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tree")
            .field("root", &self.root())
            .finish_non_exhaustive()
    }
}

impl From<Draft> for Tree {
    fn from(draft: Draft) -> Self {
        Self::from(&draft)
    }
}

impl From<&Draft> for Tree {
    fn from(draft: &Draft) -> Self {
        Self::build(|builder| builder.draft(draft))
    }
}

impl Tree {
    /// Build a tree: `build` makes nodes with the builder it is given and
    /// returns the root. The brand keeps every node it makes in this tree.
    ///
    /// A node of one build cannot become part of another tree, where its
    /// indices would name other nodes:
    ///
    /// ```compile_fail
    /// use disktree_core::tree::{Facts, NodeKind, Tree};
    /// let mut smuggled = None;
    /// let _first = Tree::build(|builder| {
    ///     smuggled =
    ///         Some(builder.leaf("x", Facts::entry(NodeKind::File, 1), None));
    ///     builder.leaf("root", Facts::directory(), None)
    /// });
    /// let _second = Tree::build(|_| smuggled.take().unwrap());
    /// ```
    pub fn build(build: impl for<'h> FnOnce(&'h Builder) -> Built<'h>) -> Self {
        let builder = Builder::default();
        let root = build(&builder).slot;
        Self::finish(builder, root)
    }

    /// [`Self::build`], for a build that can fail.
    pub fn try_build<E>(
        build: impl for<'h> FnOnce(&'h Builder) -> Result<Built<'h>, E>,
    ) -> Result<Self, E> {
        let builder = Builder::default();
        let root = build(&builder)?.slot;
        Ok(Self::finish(builder, root))
    }

    fn finish(builder: Builder, root: Slot) -> Self {
        let mut store = builder
            .store
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        store.root = Store::index(store.nodes.len());
        store.nodes.push(root);
        store.relayout();
        Self { store }
    }

    /// A tree of its own holding a copy of `node` and everything beneath:
    /// the whole store at once when `node` is a root.
    pub fn copy_of(node: Node<'_>) -> Self {
        if node.is_root() {
            return Self {
                store: node.store.clone(),
            };
        }
        Self::build(|builder| builder.copy(node))
    }

    pub fn root(&self) -> Node<'_> {
        Node {
            store: &self.store,
            slot: &self.store.nodes[self.store.root as usize],
        }
    }

    pub const fn root_mut(&mut self) -> NodeMut<'_> {
        NodeMut {
            id: self.store.root,
            store: &mut self.store,
        }
    }

    pub const fn storage_bytes(&self) -> StorageBytes {
        self.store.bytes()
    }

    /// Replace the sharing figures of the nodes at these positions, which
    /// [`Node::id`] gave on this tree, unchanged since.
    pub(crate) fn set_sharing(
        &mut self,
        figures: impl IntoIterator<Item = (u32, Option<Sharing>)>,
    ) {
        for (id, sharing) in figures {
            NodeMut {
                store: &mut self.store,
                id,
            }
            .set_sharing(sharing);
        }
    }

    /// Every leaf with `leaf`, then each directory with `directory` once
    /// its children are done; separate subtrees near the top run on
    /// separate workers.
    pub fn par_bottom_up(
        &mut self,
        leaf: impl Fn(&mut Facts) + Sync,
        directory: impl Fn(&mut Facts, Block<'_>) + Sync,
    ) {
        let Store { nodes, names, .. } = &mut self.store;
        let (root, region) =
            nodes.split_first_mut().expect("a tree has a root");
        bottom_up(root, region, 1, names, 0, &(leaf, directory));
    }

    /// The root's children with `root`, which writes the root's facts and
    /// theirs, then each directory's children with `directory` once its
    /// own facts are written, given those facts; separate subtrees near
    /// the top run on separate workers, as in [`Self::par_bottom_up`].
    pub fn par_top_down(
        &mut self,
        root: impl FnOnce(&mut Facts, Block<'_>),
        directory: impl Fn(&Facts, Block<'_>) + Sync,
    ) {
        let Store { nodes, names, .. } = &mut self.store;
        let (node, region) =
            nodes.split_first_mut().expect("a tree has a root");
        // The root's block, if it has one, is the first after the root.
        debug_assert!(node.len == 0 || node.first == 1, "a laid-out tree");
        let (block, rest) = region.split_at_mut(node.len as usize);
        root(
            &mut node.facts,
            Block {
                slots: &mut *block,
                names,
                first: node.first,
            },
        );
        let base = 1 + block.len();
        top_down(block, rest, base, names, 0, &directory);
    }

    /// Every node, spread across workers, for a pass that only reads and
    /// needs no particular order. The n-th has [`Node::id`] n.
    pub(crate) fn par_nodes(
        &self,
    ) -> impl IndexedParallelIterator<Item = Node<'_>> {
        let store = &self.store;
        store.nodes.par_iter().map(move |slot| Node { store, slot })
    }

    /// Records in each side table, for measuring what they cost.
    pub const fn side_records(&self) -> (usize, usize, usize) {
        (
            self.store.allocations.len(),
            self.store.sharings.len(),
            self.store.both.len(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(bytes: u64) -> Facts {
        Facts::entry(NodeKind::File, bytes)
    }

    fn names(node: Node<'_>) -> Vec<&str> {
        node.children().iter().map(Node::name).collect()
    }

    fn sample() -> Tree {
        Tree::build(|builder| {
            let deep = builder.leaf("deep", file(7), None);
            let nested = builder.directory(
                "child",
                Facts::directory(),
                None,
                vec![deep],
            );
            let direct = builder.leaf(
                "direct",
                file(5),
                Some(Allocation {
                    bytes: 5,
                    identity: None,
                    links: 1,
                    followed: false,
                    clone: None,
                }),
            );
            let empty = builder.leaf("", file(0), None);
            builder.directory(
                "root",
                Facts::directory(),
                None,
                vec![nested, direct, empty],
            )
        })
    }

    /// A home directory is tens of millions of nodes: every byte here is
    /// tens of megabytes there.
    #[test]
    fn a_node_stays_small() {
        // Facts, then a name offset, the first child, the child count and
        // one side-table index.
        assert_eq!(size_of::<Facts>(), 48);
        assert_eq!(size_of::<Slot>(), 64);
    }

    #[test]
    fn a_built_tree_reads_back() {
        let tree = sample();
        let root = tree.root();
        assert_eq!(root.name(), "root");
        assert_eq!(names(root), ["child", "direct", ""]);
        let deep = root.children().at(0).children().at(0);
        assert_eq!(deep.name(), "deep");
        assert_eq!(deep.bytes, 7);
        assert_eq!(
            root.children().at(1).allocation().map(|a| a.bytes),
            Some(5)
        );
        assert!(root.children().at(2).children().is_empty());
    }

    #[test]
    fn a_long_name_and_many_children_survive() {
        let long = "é".repeat(300);
        let tree = Tree::build(|builder| {
            let children = (0..1000)
                .map(|index| {
                    builder.leaf(&format!("{index}"), file(index), None)
                })
                .chain(std::iter::once(builder.leaf(&long, file(1), None)))
                .collect();
            builder.directory("root", Facts::directory(), None, children)
        });
        let children = tree.root().children();
        assert_eq!(children.len(), 1001);
        assert_eq!(children.at(999).name(), "999");
        assert_eq!(children.at(999).bytes, 999);
        assert_eq!(children.at(1000).name(), long);
    }

    #[test]
    fn writes_reach_the_tree_and_children_reorder_in_place() {
        let mut tree = sample();
        let mut root = tree.root_mut();
        root.facts_mut().bytes = 12;
        root.for_each_child_mut(|mut child| {
            child.facts_mut().read_error = true;
        });
        root.sort_children_by(|left, right| left.name.cmp(right.name));
        root.child_mut(0).expect("a child").facts_mut().modified = 9;
        let root = tree.root();
        assert_eq!(root.bytes, 12);
        assert_eq!(names(root), ["", "child", "direct"]);
        assert_eq!(root.children().at(0).modified, 9);
        assert!(root.children().iter().all(|child| child.read_error));
        assert_eq!(root.children().at(1).children().at(0).name(), "deep");
    }

    #[test]
    fn sharing_is_rewritten_in_place() {
        let mut tree = sample();
        let mut root = tree.root_mut();
        root.set_sharing(Some(Sharing::default()));
        let first = root.node().stored_sharing().map(std::ptr::from_ref);
        root.set_sharing(Some(Sharing {
            files: 3,
            ..Sharing::default()
        }));
        assert_eq!(root.node().stored_sharing().map(std::ptr::from_ref), first);
        assert_eq!(tree.root().stored_sharing().map(|s| s.files), Some(3));
        tree.root_mut().set_sharing(None);
        assert!(tree.root().stored_sharing().is_none());
    }

    #[test]
    fn a_node_can_hold_an_allocation_and_sharing() {
        let mut tree = sample();
        let mut root = tree.root_mut();
        let mut direct = root.child_mut(1).expect("direct");
        // What aggregating does to every file: no figures to keep must
        // not cost the file its allocation.
        direct.set_sharing(None);
        assert_eq!(direct.node().allocation().map(|a| a.bytes), Some(5));
        direct.set_sharing(Some(Sharing {
            files: 2,
            ..Sharing::default()
        }));
        let node = direct.node();
        assert_eq!(node.allocation().map(|a| a.bytes), Some(5));
        assert_eq!(node.stored_sharing().map(|s| s.files), Some(2));
        direct.set_sharing(None);
        assert_eq!(direct.node().allocation().map(|a| a.bytes), Some(5));
        assert!(direct.node().stored_sharing().is_none());
    }

    #[test]
    fn a_copy_outlives_its_source() {
        let source = sample();
        let copy = source.clone();
        drop(source);
        assert_eq!(copy.root().children().len(), 3);
        assert_eq!(
            copy.root().children().at(0).children().at(0).name(),
            "deep"
        );
        let subtree = {
            let source = sample();
            Tree::copy_of(source.root().children().at(0))
        };
        assert_eq!(subtree.root().name(), "child");
        assert_eq!(subtree.root().children().len(), 1);
    }

    /// Everything below two nodes reads the same, names included.
    fn assert_same(left: Node<'_>, right: Node<'_>) {
        assert_eq!(left.bytes, right.bytes, "{}", left.name());
        assert_eq!(left.kind, right.kind, "{}", left.name());
        assert_eq!(
            left.allocation().map(|a| a.bytes),
            right.allocation().map(|a| a.bytes),
            "{}",
            left.name()
        );
        assert_eq!(
            left.stored_sharing().map(|s| s.files),
            right.stored_sharing().map(|s| s.files),
            "{}",
            left.name()
        );
        assert_eq!(names(left), names(right));
        for (left, right) in left.children().iter().zip(right.children()) {
            assert_same(left, right);
        }
    }

    #[test]
    fn a_whole_tree_copied_into_a_build_keeps_its_records() {
        let mut source = sample();
        let shared = |files| {
            Some(Sharing {
                files,
                ..Sharing::default()
            })
        };
        let mut root = source.root_mut();
        root.set_sharing(shared(1));
        // `direct` already has an allocation: this puts it in `both`.
        root.child_mut(1).expect("direct").set_sharing(shared(2));
        root.child_mut(0).expect("child").set_sharing(shared(3));
        let allocation = Allocation {
            bytes: 9,
            identity: None,
            links: 1,
            followed: false,
            clone: None,
        };
        let tree = Tree::build(|builder| {
            // Records already in every table, so the copy's must move.
            let mut before = builder.leaf("before", file(9), Some(allocation));
            before.slot.extra =
                builder.lock().set_sharing(before.slot.extra, shared(4));
            let other = builder.leaf("other", file(1), None);
            let other = builder.directory(
                "other-dir",
                Facts::directory(),
                None,
                vec![other],
            );
            let reused = builder.copy_named(source.root(), "reused");
            builder.directory(
                "top",
                Facts::directory(),
                None,
                vec![before, other, reused],
            )
        });
        let reused = tree.root().children().at(2);
        assert_eq!(reused.name(), "reused");
        assert_same(reused, source.root());
        let before = tree.root().children().at(0);
        assert_eq!(before.allocation().map(|a| a.bytes), Some(9));
        assert_eq!(before.stored_sharing().map(|s| s.files), Some(4));
        // The source's root went in too, and nothing reaches it.
        assert_eq!(tree.store.nodes.len(), 1 + 3 + 1 + 3 + 1);
    }

    #[test]
    fn a_tree_moves_to_another_thread() {
        let tree = sample();
        let count = std::thread::spawn(move || tree.root().children().len())
            .join()
            .expect("joined");
        assert_eq!(count, 3);
    }

    /// Deeper than the levels that run in parallel, and with every block
    /// re-sorted so blocks no longer list children in the order their runs
    /// were laid out.
    #[test]
    fn top_down_writes_every_node_once_from_its_parent() {
        fn draft(name: &str, depth: usize) -> Draft {
            let mut node = Draft::directory(name);
            if depth < crate::tree::PARALLEL_LEVELS + 3 {
                node.children = (0..3)
                    .map(|index| draft(&format!("{name}/{index}"), depth + 1))
                    .collect();
            }
            node.children.push(Draft::entry(
                format!("{name}/file").as_str(),
                NodeKind::File,
                0,
            ));
            node.children
                .push(Draft::directory(format!("{name}/empty").as_str()));
            node
        }
        fn check(node: Node<'_>, depth: i64, nodes: &mut usize) {
            *nodes += 1;
            assert_eq!(node.bytes, 1, "{} written once", node.name());
            assert_eq!(node.modified, depth, "{} at its depth", node.name());
            for child in node.children() {
                check(child, depth + 1, nodes);
            }
        }
        let mut tree = Tree::from(draft("root", 0));
        tree.par_bottom_up(
            |_| {},
            |_, mut block| {
                block.sort_by(|left, right| right.name.cmp(left.name));
            },
        );
        // Each write counts itself in `bytes` and records its depth in
        // `modified`, which only the parent's own facts can give.
        tree.par_top_down(
            |root, mut block| {
                root.bytes += 1;
                root.modified = 0;
                block.for_each_facts_mut(|child| {
                    child.bytes += 1;
                    child.modified = 1;
                });
            },
            |parent, mut block| {
                block.for_each_facts_mut(|child| {
                    child.bytes += 1;
                    child.modified = parent.modified + 1;
                });
            },
        );
        let mut nodes = 0;
        check(tree.root(), 0, &mut nodes);
        assert_eq!(nodes, tree.store.nodes.len());
    }

    #[test]
    fn a_tree_built_on_every_worker_holds_every_node() {
        let tree = Tree::build(|builder| {
            let children = (0..64)
                .into_par_iter()
                .map(|dir| {
                    let names: Vec<String> = (0..100)
                        .map(|index| format!("{dir}-{index}"))
                        .collect();
                    let files = builder.leaves(
                        names.iter().map(|name| (name.as_str(), file(1), None)),
                    );
                    builder.directory(
                        &format!("{dir}"),
                        Facts::directory(),
                        None,
                        files,
                    )
                })
                .collect();
            builder.directory("root", Facts::directory(), None, children)
        });
        let root = tree.root();
        assert_eq!(root.children().len(), 64);
        assert!(root.children().iter().enumerate().all(|(dir, child)| {
            child.children().len() == 100
                && child.children().at(99).name() == format!("{dir}-99")
        }));
        let bytes = tree.storage_bytes();
        assert!(bytes.used > 0 && bytes.used <= bytes.reserved);
    }
}

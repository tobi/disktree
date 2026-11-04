//! An [`Allocation`] as a tree stores it: in the smallest record that
//! holds what the node does not already say.
//!
//! A scan keeps an allocation for every file that may share blocks, and
//! as a 96-byte struct that was more than the nodes themselves cost. On
//! 16.4 million files of agent worktrees, 14.2 million kept one: 97.7 %
//! were clones with one name whose size is the node's own and whose
//! data fork is all of it, and nearly all of those were wholly shared.
//! Such a clone keeps 16 or 24 bytes here. A file with several names, or
//! one reached through a followed link, keeps 16 when it has no clone
//! facts: its identity is the node's inode. Anything else keeps the whole
//! struct, so no figure is ever approximated to fit.

use rustc_hash::FxHashMap;

use super::Facts;
use crate::sharing::{Allocation, CloneInfo};

/// The tags of [`super::Extra`] that name one of these tables.
pub(super) const SHARED: u32 = 1;
pub(super) const CLONE: u32 = 2;
pub(super) const LINK: u32 = 3;
pub(super) const FULL: u32 = 4;

/// Whether `tag` names one of these tables.
pub(super) const fn holds(tag: u32) -> bool {
    matches!(tag, SHARED | CLONE | LINK | FULL)
}

/// Whether the table `tag` names reads a file's size from its node, which
/// holds only while the leaf has no inode for de-duplication to act on.
pub(super) const fn reads_size(tag: u32) -> bool {
    matches!(tag, SHARED | CLONE)
}

/// Where each table of one store starts once it is appended to another:
/// the other's lengths before the append.
#[derive(Clone, Copy, Debug)]
pub(super) struct Starts {
    shared: usize,
    clones: usize,
    links: usize,
    full: usize,
}

impl Starts {
    /// Where the record at `index` of the table `tag` names moved to.
    pub(super) const fn moved(self, tag: u32, index: usize) -> usize {
        index
            + match tag {
                SHARED => self.shared,
                CLONE => self.clones,
                LINK => self.links,
                FULL => self.full,
                _ => 0,
            }
    }
}

/// A clone with one name and no private blocks: what nearly every file
/// of a directory of cloned worktrees is. Every file of a clone group
/// reads the same one, which is kept once: see [`Records::reuse`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct SharedClone {
    id: u64,
    /// APFS reports the real device as 32 bits.
    device: u32,
    references: u32,
}

/// A clone with one name whose private blocks are some, or unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PartialClone {
    id: u64,
    /// [`UNKNOWN`] when the file system did not say.
    private: u64,
    device: u32,
    references: u32,
}

/// Shared records [`Records::reuse`] remembers while a tree is built:
/// a group a record, so room for this many groups. Past it a record is
/// kept per file again, which costs memory, not accuracy.
const REUSED: usize = 1 << 20;

/// No file holds `u64::MAX` bytes, so it can stand for an unknown
/// private size without an `Option`'s eight bytes of tag.
const UNKNOWN: u64 = u64::MAX;

/// A file with more than one name, or one a scan that follows links
/// reached, with no clone facts: its own size, which de-duplication can
/// take from the node, and how many names it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Linked {
    bytes: u64,
    links: u32,
    followed: bool,
}

/// The identity sharing reads: only a file with more than one name is
/// matched to its other names, and never through a followed link.
fn identity(facts: &Facts, links: u64, followed: bool) -> Option<(u64, u64)> {
    if links > 1 && !followed {
        facts.inode.map(|(device, inode)| (device, inode.get()))
    } else {
        None
    }
}

/// What the stored record is, before it goes into its table.
enum Packed {
    SharedClone(SharedClone),
    PartialClone(PartialClone),
    Linked(Linked),
    Full(Allocation),
}

impl Packed {
    /// The smallest record that gives `allocation` back for `facts`.
    ///
    /// A clone's size is taken from the node only when nothing can change
    /// the node's: a leaf with no inode is never de-duplicated, which is
    /// the one thing that rewrites a leaf's `bytes`.
    fn of(allocation: Allocation, facts: &Facts) -> Self {
        #[cfg(test)]
        if WHOLE.get() {
            return Self::Full(allocation);
        }
        let Allocation {
            bytes,
            links,
            followed,
            clone,
            ..
        } = allocation;
        let identity_kept = allocation.identity
            == identity(facts, links, followed)
            || links <= 1
            || followed;
        match clone {
            Some(info)
                if links == 1
                    && !followed
                    && !facts.kind.is_dir()
                    && facts.inode.is_none()
                    && bytes == facts.bytes
                    && info.data_bytes == facts.bytes
                    && info.private_bytes != Some(UNKNOWN) =>
            {
                let Ok(device) = u32::try_from(info.device) else {
                    return Self::Full(allocation);
                };
                if info.private_bytes == Some(0) {
                    Self::SharedClone(SharedClone {
                        id: info.id,
                        device,
                        references: info.references,
                    })
                } else {
                    Self::PartialClone(PartialClone {
                        id: info.id,
                        private: info.private_bytes.unwrap_or(UNKNOWN),
                        device,
                        references: info.references,
                    })
                }
            }
            None if identity_kept => match u32::try_from(links) {
                Ok(links) => Self::Linked(Linked {
                    bytes,
                    links,
                    followed,
                }),
                Err(_) => Self::Full(allocation),
            },
            _ => Self::Full(allocation),
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Keep every record whole, to compare figures against.
    static WHOLE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// `build` with every allocation it keeps on this thread kept whole.
#[cfg(test)]
pub fn kept_whole<R>(build: impl FnOnce() -> R) -> R {
    WHOLE.set(true);
    let built = build();
    WHOLE.set(false);
    built
}

const fn clone_of(facts: &Facts, info: CloneInfo) -> Allocation {
    Allocation {
        bytes: facts.bytes,
        identity: None,
        links: 1,
        followed: false,
        clone: Some(info),
    }
}

/// Every allocation of a tree, one table for each record.
#[derive(Default)]
pub(super) struct Records {
    shared: Vec<SharedClone>,
    clones: Vec<PartialClone>,
    links: Vec<Linked>,
    full: Vec<Allocation>,
    /// Where each shared record is, while the tree is built: files of a
    /// group share a record, as no record is ever changed in place. On
    /// 8.9 million files of agent worktrees 8.1 million kept one, in 129
    /// thousand groups; one apiece was 124 MiB of a 1 GiB tree. Dropped
    /// when the tree is finished.
    reuse: FxHashMap<SharedClone, u32>,
}

impl Records {
    /// Keep `allocation` for a node of `facts`: the tag of the table it
    /// went into and its index there.
    pub(super) fn push(
        &mut self,
        allocation: Allocation,
        facts: &Facts,
    ) -> (u32, usize) {
        match Packed::of(allocation, facts) {
            Packed::SharedClone(record) => {
                if let Some(&index) = self.reuse.get(&record) {
                    return (SHARED, index as usize);
                }
                let index = self.shared.len();
                self.shared.push(record);
                if self.reuse.len() < REUSED
                    && let Ok(at) = u32::try_from(index)
                {
                    self.reuse.insert(record, at);
                }
                (SHARED, index)
            }
            Packed::PartialClone(record) => {
                self.clones.push(record);
                (CLONE, self.clones.len() - 1)
            }
            Packed::Linked(record) => {
                self.links.push(record);
                (LINK, self.links.len() - 1)
            }
            Packed::Full(record) => {
                self.full.push(record);
                (FULL, self.full.len() - 1)
            }
        }
    }

    /// The allocation at `index` of the table `tag` names, for the node
    /// of `facts` it was kept for.
    pub(super) fn get(
        &self,
        tag: u32,
        index: usize,
        facts: &Facts,
    ) -> Option<Allocation> {
        Some(match tag {
            SHARED => {
                let SharedClone {
                    id,
                    device,
                    references,
                } = self.shared[index];
                clone_of(
                    facts,
                    CloneInfo {
                        device: u64::from(device),
                        id,
                        references,
                        private_bytes: Some(0),
                        data_bytes: facts.bytes,
                    },
                )
            }
            CLONE => {
                let PartialClone {
                    id,
                    private,
                    device,
                    references,
                } = self.clones[index];
                clone_of(
                    facts,
                    CloneInfo {
                        device: u64::from(device),
                        id,
                        references,
                        private_bytes: (private != UNKNOWN).then_some(private),
                        data_bytes: facts.bytes,
                    },
                )
            }
            LINK => {
                let Linked {
                    bytes,
                    links,
                    followed,
                } = self.links[index];
                let links = u64::from(links);
                Allocation {
                    bytes,
                    identity: identity(facts, links, followed),
                    links,
                    followed,
                    clone: None,
                }
            }
            FULL => self.full[index],
            _ => return None,
        })
    }

    /// Forget which records can be shared: the tree is finished.
    pub(super) fn seal(&mut self) {
        self.reuse = FxHashMap::default();
    }

    /// Where each table of a store appended to this one will start.
    pub(super) const fn starts(&self) -> Starts {
        Starts {
            shared: self.shared.len(),
            clones: self.clones.len(),
            links: self.links.len(),
            full: self.full.len(),
        }
    }

    pub(super) fn append(&mut self, other: &Self) {
        self.shared.extend_from_slice(&other.shared);
        self.clones.extend_from_slice(&other.clones);
        self.links.extend_from_slice(&other.links);
        self.full.extend_from_slice(&other.full);
    }

    /// A copy made on every worker, as [`super::Store`]'s is.
    pub(super) fn par_copy(&self) -> Self {
        use super::copied;
        let ((shared, clones), (links, full)) = rayon::join(
            || rayon::join(|| copied(&self.shared), || copied(&self.clones)),
            || rayon::join(|| copied(&self.links), || copied(&self.full)),
        );
        Self {
            shared,
            clones,
            links,
            full,
            reuse: FxHashMap::default(),
        }
    }

    /// Records kept, in all tables.
    pub(super) const fn len(&self) -> usize {
        self.shared.len()
            + self.clones.len()
            + self.links.len()
            + self.full.len()
    }

    /// Bytes the records fill, and bytes the tables have room for.
    pub(super) const fn bytes(&self) -> (usize, usize) {
        (
            self.shared.len() * size_of::<SharedClone>()
                + self.clones.len() * size_of::<PartialClone>()
                + self.links.len() * size_of::<Linked>()
                + self.full.len() * size_of::<Allocation>(),
            self.shared.capacity() * size_of::<SharedClone>()
                + self.clones.capacity() * size_of::<PartialClone>()
                + self.links.capacity() * size_of::<Linked>()
                + self.full.capacity() * size_of::<Allocation>(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;
    use crate::tree::NodeKind;

    fn inode(device: u64, inode: u64) -> Option<(u64, NonZeroU64)> {
        Some((device, NonZeroU64::new(inode)?))
    }

    /// Fourteen million records on one real tree: every byte here is
    /// fourteen megabytes there.
    #[test]
    fn an_allocation_record_stays_small() {
        assert_eq!(size_of::<SharedClone>(), 16);
        assert_eq!(size_of::<PartialClone>(), 24);
        assert_eq!(size_of::<Linked>(), 16);
        // Kept whole only for what the small records cannot say.
        assert_eq!(size_of::<Allocation>(), 96);
    }

    fn info(private: Option<u64>) -> CloneInfo {
        CloneInfo {
            device: 16_777_234,
            id: 0xC0FF_EE00_0000_0001,
            references: 70_000,
            private_bytes: private,
            data_bytes: 4096,
        }
    }

    fn clone(private: Option<u64>) -> Allocation {
        Allocation {
            bytes: 4096,
            // A listing reports one; sharing never reads it for a file
            // with one name, so the record does not keep it.
            identity: Some((1, 7)),
            links: 1,
            followed: false,
            clone: Some(info(private)),
        }
    }

    fn file(bytes: u64) -> Facts {
        Facts::entry(NodeKind::File, bytes)
    }

    /// Kept, then read back: the table it went into and what it reads as.
    fn kept(allocation: Allocation, facts: &Facts) -> (u32, Allocation) {
        let mut records = Records::default();
        let (tag, index) = records.push(allocation, facts);
        (tag, records.get(tag, index, facts).expect("a record"))
    }

    #[test]
    fn a_clone_with_one_name_keeps_only_what_its_node_does_not_say() {
        let facts = file(4096);
        for (private, tag) in [
            (Some(0), SHARED),
            (Some(1024), CLONE),
            (Some(4096), CLONE),
            (None, CLONE),
        ] {
            let (stored, back) = kept(clone(private), &facts);
            assert_eq!(stored, tag, "{private:?}");
            assert_eq!(
                back,
                Allocation {
                    identity: None,
                    ..clone(private)
                },
                "{private:?}"
            );
        }
    }

    #[test]
    fn files_of_a_group_share_one_record_until_the_tree_is_sealed() {
        let mut records = Records::default();
        let small = file(4096);
        let large = file(8192);
        let mut larger = clone(Some(0));
        larger.bytes = 8192;
        larger.clone.as_mut().expect("clone").data_bytes = 8192;
        let (tag, first) = records.push(clone(Some(0)), &small);
        assert_eq!(tag, SHARED);
        // Another member, another size: the size is the node's own.
        assert_eq!(records.push(larger, &large), (SHARED, first));
        assert_eq!(
            records.get(SHARED, first, &large),
            Some(Allocation {
                identity: None,
                ..larger
            })
        );
        // Another reference count is another record.
        let mut counted = clone(Some(0));
        counted.clone.as_mut().expect("clone").references = 3;
        assert_ne!(records.push(counted, &small), (SHARED, first));
        assert_eq!(records.len(), 2);
        records.seal();
        assert_ne!(records.push(clone(Some(0)), &small), (SHARED, first));
    }

    #[test]
    fn hardlinks_and_followed_links_keep_their_own_size_and_names() {
        let mut facts = file(0);
        facts.inode = inode(1, 7);
        // A second name de-duplication has already charged elsewhere.
        let hardlink = Allocation {
            bytes: 4096,
            identity: Some((1, 7)),
            links: 3,
            followed: false,
            clone: None,
        };
        assert_eq!(kept(hardlink, &facts), (LINK, hardlink));
        let followed = Allocation {
            identity: None,
            links: 1,
            followed: true,
            ..hardlink
        };
        assert_eq!(kept(followed, &facts), (LINK, followed));
        let directory = Allocation {
            bytes: 0,
            ..followed
        };
        assert_eq!(kept(directory, &Facts::directory()), (LINK, directory));
    }

    /// Whatever a small record would have to guess at is kept whole, and
    /// reads back exactly as it went in.
    #[test]
    fn what_a_small_record_cannot_say_is_kept_whole() {
        let facts = file(4096);
        let mut tracked = file(4096);
        tracked.inode = inode(1, 7);
        let mut cases = vec![
            // De-duplication could zero the node's size.
            (clone(Some(0)), tracked),
            // A resource fork beside the cloned data fork.
            (
                Allocation {
                    clone: Some(CloneInfo {
                        data_bytes: 1024,
                        ..info(Some(0))
                    }),
                    ..clone(Some(0))
                },
                facts,
            ),
            // A clone with more than one name.
            (
                Allocation {
                    links: 2,
                    identity: Some((1, 7)),
                    ..clone(Some(0))
                },
                tracked,
            ),
            // A device wider than APFS reports.
            (
                Allocation {
                    clone: Some(CloneInfo {
                        device: 1 << 40,
                        ..info(Some(0))
                    }),
                    ..clone(Some(0))
                },
                facts,
            ),
            // A private size the sentinel would misread.
            (clone(Some(UNKNOWN)), facts),
            // More names than the small record counts.
            (
                Allocation {
                    links: 1 << 33,
                    identity: Some((1, 7)),
                    clone: None,
                    ..clone(None)
                },
                tracked,
            ),
            // An identity that is not the node's inode.
            (
                Allocation {
                    links: 2,
                    identity: Some((1, 8)),
                    clone: None,
                    ..clone(None)
                },
                tracked,
            ),
        ];
        // A clone's size differs from its node's.
        cases.push((clone(Some(0)), file(8192)));
        // A directory's size is its children's.
        cases.push((
            clone(Some(0)),
            Facts {
                bytes: 4096,
                ..Facts::directory()
            },
        ));
        for (allocation, facts) in cases {
            assert_eq!(
                kept(allocation, &facts),
                (FULL, allocation),
                "{allocation:?}"
            );
        }
    }
}

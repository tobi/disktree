//! Per-scan reuse of confirmed full-clone metadata, without a serial consumer.

use std::hash::{Hash, Hasher as _};
use std::sync::Mutex;

use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

use crate::sharing::CloneInfo;

const SHARDS: usize = 64;
const GROUPS_PER_SHARD: usize = 1_000_000 / SHARDS;
const ALIASES_PER_SHARD: usize = GROUPS_PER_SHARD;

struct Sample {
    info: CloneInfo,
    remaining: u32,
    aliases: FxHashSet<u64>,
}

#[derive(Default)]
struct Shard {
    groups: FxHashMap<(u64, u64), Sample>,
    aliases: usize,
}

pub struct Cache {
    shards: [Mutex<Shard>; SHARDS],
    group_limit: usize,
    alias_limit: usize,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            shards: std::array::from_fn(|_| Mutex::new(Shard::default())),
            group_limit: GROUPS_PER_SHARD,
            alias_limit: ALIASES_PER_SHARD,
        }
    }
}

impl Cache {
    fn shard(&self, key: (u64, u64)) -> &Mutex<Shard> {
        let mut hash = FxHasher::default();
        key.hash(&mut hash);
        let index = usize::try_from(hash.finish() % SHARDS as u64)
            .expect("shard index fits");
        &self.shards[index]
    }

    /// Whether another file of this group was found to share all its
    /// blocks: only such a file is sampled. For a listing that leaves the
    /// sharing flags out (`macos::Clones::Ids`), which every file of a
    /// group shares. A group past the cache's room is not known here, and
    /// its files read their own flags: that costs time, not accuracy.
    pub fn shares_all(&self, device: u64, id: u64) -> bool {
        let key = (device, id);
        device != 0
            && id != 0
            && self
                .shard(key)
                .lock()
                .expect("clone cache")
                .groups
                .contains_key(&key)
    }

    /// A reference count is a reuse budget, not a permanent fact. A later
    /// member past that budget is measured again, catching clone growth.
    /// Modified clones and unknown physical volumes are never reused.
    pub fn get(
        &self,
        device: u64,
        id: u64,
        inode: u64,
        links: u64,
        allocated: u64,
        measure: impl FnOnce() -> CloneInfo,
    ) -> CloneInfo {
        let key = (device, id);
        if device != 0 && id != 0 && inode != 0 {
            let mut shard = self.shard(key).lock().expect("clone cache");
            let alias_room = shard.aliases < self.alias_limit;
            if let Some(sample) = shard
                .groups
                .get_mut(&key)
                .filter(|sample| sample.info.data_bytes == allocated)
            {
                if sample.aliases.contains(&inode) {
                    return sample.info;
                }
                if sample.remaining > 0 && (links == 1 || alias_room) {
                    sample.remaining -= 1;
                    let info = sample.info;
                    if links > 1 && sample.aliases.insert(inode) {
                        shard.aliases += 1;
                    }
                    return info;
                }
            }
            drop(shard);
        }
        // Filesystem calls never hold a shard lock. Racing first samples
        // may duplicate a query; they cannot funnel directory workers into
        // one slow reader. Each scan owns its cache, including re-scans.
        let info = measure();
        if device == 0
            || inode == 0
            || info.device != device
            || info.id != id
            || info.references < 2
            || info.private_bytes != Some(0)
        {
            return info;
        }
        let mut shard = self.shard(key).lock().expect("clone cache");
        if let Some(old) = shard.groups.remove(&key) {
            shard.aliases -= old.aliases.len();
        }
        if shard.groups.len() < self.group_limit
            && (links == 1 || shard.aliases < self.alias_limit)
        {
            let mut aliases = FxHashSet::default();
            if links > 1 {
                aliases.insert(inode);
                shard.aliases += 1;
            }
            shard.groups.insert(
                key,
                Sample {
                    info,
                    remaining: info.references - 1,
                    aliases,
                },
            );
        }
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn sample(device: u64, id: u64, references: u32) -> CloneInfo {
        CloneInfo {
            device,
            id,
            references,
            private_bytes: Some(0),
            data_bytes: 4096,
        }
    }

    #[test]
    fn volume_scope_scan_lifetime_and_memory_caps_limit_reuse() {
        let cache = Cache {
            group_limit: 1,
            alias_limit: 1,
            ..Cache::default()
        };
        let calls = Cell::new(0);
        for device in 1..=128 {
            for inode in 1..=3 {
                let value = cache.get(device, 42, inode, 2, 4096, || {
                    calls.set(calls.get() + 1);
                    sample(device, 42, 3)
                });
                assert_eq!(value.device, device);
            }
        }
        for shard in &cache.shards {
            let shard = shard.lock().expect("cache");
            assert!(shard.groups.len() <= 1);
            assert!(shard.aliases <= 1);
            drop(shard);
        }
        assert!(calls.get() > 128, "capped aliases must be measured");
        let fresh = Cache::default();
        let mut measured = false;
        fresh.get(1, 42, 1, 1, 4096, || {
            measured = true;
            sample(1, 42, 3)
        });
        assert!(measured, "new scans must not reuse previous observations");
    }

    #[test]
    fn only_a_sampled_group_is_known_to_share_all_its_blocks() {
        let cache = Cache::default();
        assert!(!cache.shares_all(1, 42));
        cache.get(1, 42, 1, 1, 4096, || sample(1, 42, 2));
        assert!(cache.shares_all(1, 42));
        // A modified member is never a sample, nor is an unknown volume.
        cache.get(1, 43, 2, 1, 4096, || {
            let mut info = sample(1, 43, 2);
            info.private_bytes = Some(4096);
            info
        });
        assert!(!cache.shares_all(1, 43));
        cache.get(0, 44, 3, 1, 4096, || sample(0, 44, 2));
        assert!(!cache.shares_all(0, 44));
    }

    #[test]
    fn different_all_fork_allocation_requires_a_fresh_private_measurement() {
        let cache = Cache::default();
        cache.get(1, 42, 1, 1, 4096, || sample(1, 42, 2));
        let mut measured = false;
        let info = cache.get(1, 42, 2, 1, 8192, || {
            measured = true;
            let mut info = sample(1, 42, 2);
            info.private_bytes = Some(4096);
            info
        });
        assert!(
            measured,
            "a resource fork must not inherit another member's zero private size"
        );
        assert_eq!(info.private_bytes, Some(4096));
    }

    #[test]
    fn reference_budget_refreshes_and_hardlink_aliases_do_not_spend_it() {
        let cache = Cache::default();
        let calls = Cell::new(0);
        for (inode, links, refs) in [(1, 2, 2), (1, 2, 2), (2, 1, 2), (3, 1, 3)]
        {
            let info = cache.get(1, 42, inode, links, 4096, || {
                calls.set(calls.get() + 1);
                sample(1, 42, refs)
            });
            assert_eq!(info.references, refs);
        }
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn unknown_volumes_and_modified_members_are_measured_individually() {
        for device in [0, 1] {
            let cache = Cache::default();
            let calls = Cell::new(0);
            for inode in 1..=8 {
                cache.get(device, 42, inode, 1, 4096, || {
                    calls.set(calls.get() + 1);
                    let mut info = sample(device, 42, 8);
                    info.private_bytes = Some(4096);
                    info
                });
            }
            assert_eq!(calls.get(), 8);
        }
    }

    #[test]
    fn clone_groups_bound_queries_as_both_groups_and_members_grow() {
        for n in [128_u32, 1024] {
            let cache = Cache::default();
            let calls = Cell::new(0);
            for group in 1..=n {
                for inode in 1..=n {
                    cache.get(
                        1,
                        u64::from(group),
                        u64::from(inode),
                        1,
                        4096,
                        || {
                            calls.set(calls.get() + 1);
                            sample(1, u64::from(group), n)
                        },
                    );
                }
            }
            assert_eq!(calls.get(), n, "one query per confirmed group");
        }
    }
}

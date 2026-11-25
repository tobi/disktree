//! Persistent Protobuf cache database backed by `redb`.
//!
//! Models each host's full canonical filesystem tree rooted at `/` (or Windows
//! volume roots). Partial scans of any subdirectory graft into the host tree
//! as adaptive Protobuf `SubtreeChunk`s, enabling:
//!
//! - O(1) skip checks for fresh subtrees during wider scans,
//! - sub-millisecond narrowing queries into already-scanned descendants,
//! - targeted subtree invalidation (preserving untouched fresh siblings),
//! - automatic graceful fallback on `ENOSPC`, `EROFS`, `EACCES`, low free
//!   space, or corrupt DB files.

use std::io;
use std::path::{Component, Path, PathBuf};

#[cfg(feature = "cache-db")]
use redb::{
    Database, ReadableTable as _, ReadableTableMetadata as _, TableDefinition,
};
use rustc_hash::{FxHashMap, FxHashSet};

#[cfg(feature = "cache-db")]
use crate::mounts::check_cache_headroom;
#[cfg(feature = "cache-db")]
use crate::proto::{
    CODEC_LZ4, Coverage, DbHeader, DirIndexEntry, FORMAT_VERSION, HostRecord,
    SubtreeChunk, decode_framed, decode_subtree_chunk, encode_framed,
    encode_subtree_chunk,
};
#[cfg(feature = "cache-db")]
use crate::tree::NodeId;
use crate::tree::TreeArena;
use crate::types::{
    CachePolicy, CacheStats, FallbackReason, Freshness, HostKey, ScanOptions,
};
#[cfg(feature = "cache-db")]
use crate::types::{SubtreeStatus, unix_now};

/// Directories with at least this many nodes become independent shards so
/// partial updates never rewrite unrelated subtrees.
#[cfg(feature = "cache-db")]
const SHARD_NODE_THRESHOLD: u32 = 2_048;

/// Shallow directories (depth <= 2 from scan root) with at least this many
/// nodes also become shards so top-level folders (e.g. `~/Downloads`, `~/src`)
/// can be skipped or invalidated independently.
#[cfg(feature = "cache-db")]
const SHALLOW_SHARD_DEPTH: usize = 2;
#[cfg(feature = "cache-db")]
const SHALLOW_SHARD_MIN_NODES: u32 = 64;

#[cfg(feature = "cache-db")]
const META_TABLE: TableDefinition<'_, &str, &[u8]> =
    TableDefinition::new("meta");
#[cfg(feature = "cache-db")]
const HOSTS_TABLE: TableDefinition<'_, &str, &[u8]> =
    TableDefinition::new("hosts");
#[cfg(feature = "cache-db")]
const PATH_TO_DIR_TABLE: TableDefinition<'_, (&str, &str), u64> =
    TableDefinition::new("path_to_dir");
#[cfg(feature = "cache-db")]
const DIR_INDEX_TABLE: TableDefinition<'_, (&str, u64), &[u8]> =
    TableDefinition::new("dir_index");
#[cfg(feature = "cache-db")]
const DIR_CHILDREN_TABLE: TableDefinition<'_, (&str, u64, u64), ()> =
    TableDefinition::new("dir_children");
#[cfg(feature = "cache-db")]
const CHUNKS_TABLE: TableDefinition<'_, (&str, u64), &[u8]> =
    TableDefinition::new("chunks");

/// Result of reading a cached subtree from [`CacheDb`].
#[derive(Debug)]
pub struct CachedSubtree {
    pub arena: TreeArena,
    pub freshness: Freshness,
}

/// Summary of an invalidation operation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InvalidateReport {
    pub invalidated_dirs: u64,
    pub touched_ancestors: u64,
}

/// Persistent (or temporary-file) cache database handle.
#[derive(Debug)]
pub struct CacheDb {
    #[cfg(feature = "cache-db")]
    db: Database,
    path: PathBuf,
    temp_file: bool,
    #[cfg(feature = "cache-db")]
    min_free_headroom: u64,
}

impl Drop for CacheDb {
    fn drop(&mut self) {
        if self.temp_file {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl CacheDb {
    /// Open or create a persistent cache DB at `path`, checking free-space
    /// headroom first.
    #[cfg(feature = "cache-db")]
    pub fn open(
        path: PathBuf,
        min_free_headroom: u64,
        temp_file: bool,
    ) -> Result<Self, FallbackReason> {
        check_cache_headroom(&path, min_free_headroom)?;

        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            return Err(classify_io_error(&err));
        }

        let db = match Database::create(&path) {
            Ok(db) => db,
            Err(err) => return Err(classify_redb_error(&err.into())),
        };

        let instance = Self {
            db,
            path,
            temp_file,
            min_free_headroom,
        };
        instance.init_schema()?;
        Ok(instance)
    }

    #[cfg(not(feature = "cache-db"))]
    pub fn open(
        _path: PathBuf,
        _min_free_headroom: u64,
        _temp_file: bool,
    ) -> Result<Self, FallbackReason> {
        Err(FallbackReason::DatabaseUnavailable {
            detail: "compiled without cache-db feature".to_owned(),
        })
    }

    /// Create an ephemeral cache DB in the OS temp directory (deleted on drop).
    pub fn open_temp(min_free_headroom: u64) -> Result<Self, FallbackReason> {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let path = std::env::temp_dir()
            .join(format!("disktree-cache-{pid}-{nanos}.redb"));
        Self::open(path, min_free_headroom, true)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn is_temp(&self) -> bool {
        self.temp_file
    }

    #[cfg(feature = "cache-db")]
    fn init_schema(&self) -> Result<(), FallbackReason> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| classify_redb_error(&e.into()))?;
        {
            let mut meta = txn
                .open_table(META_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;
            let _ = txn
                .open_table(HOSTS_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;
            let _ = txn
                .open_table(PATH_TO_DIR_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;
            let _ = txn
                .open_table(DIR_INDEX_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;
            let _ = txn
                .open_table(DIR_CHILDREN_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;
            let _ = txn
                .open_table(CHUNKS_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;

            let existing = meta
                .get("header")
                .map_err(|e| classify_redb_error(&e.into()))?
                .map(|v| v.value().to_vec());

            if let Some(bytes) = existing {
                let header: DbHeader = decode_framed(&bytes).map_err(|e| {
                    FallbackReason::DatabaseUnavailable {
                        detail: format!("corrupt cache header: {e}"),
                    }
                })?;
                if header.format_version != FORMAT_VERSION {
                    return Err(FallbackReason::DatabaseUnavailable {
                        detail: format!(
                            "incompatible cache format version {} (expected {})",
                            header.format_version, FORMAT_VERSION
                        ),
                    });
                }
            } else {
                let header = DbHeader {
                    format_version: FORMAT_VERSION,
                    created_at_unix: unix_now(),
                    codec: u32::from(CODEC_LZ4),
                };
                let bytes = encode_framed(&header);
                meta.insert("header", bytes.as_slice())
                    .map_err(|e| classify_redb_error(&e.into()))?;
                meta.insert("next_id", 1u64.to_le_bytes().as_slice())
                    .map_err(|e| classify_redb_error(&e.into()))?;
            }
        }
        txn.commit().map_err(|e| classify_redb_error(&e.into()))?;
        Ok(())
    }

    /// Persist a scanned [`TreeArena`] into the host's whole-filesystem tree,
    /// replacing any previous data strictly at or beneath `arena.root_path()`.
    #[cfg(feature = "cache-db")]
    pub fn store_subtree(
        &self,
        host: &HostKey,
        arena: &TreeArena,
        options: &ScanOptions,
        scanned_at: u64,
    ) -> Result<(), FallbackReason> {
        check_cache_headroom(&self.path, self.min_free_headroom)?;

        let host_key = host.as_db_key();
        let canonical_root = normalize_path(arena.root_path());
        let counts = arena.subtree_counts();
        let shard_nodes = choose_shard_roots(arena, &counts);

        let txn = self
            .db
            .begin_write()
            .map_err(|e| classify_redb_error(&e.into()))?;
        {
            let mut meta = txn
                .open_table(META_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;
            let mut hosts = txn
                .open_table(HOSTS_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;
            let mut path_to_dir = txn
                .open_table(PATH_TO_DIR_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;
            let mut dir_index = txn
                .open_table(DIR_INDEX_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;
            let mut dir_children = txn
                .open_table(DIR_CHILDREN_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;
            let mut chunks = txn
                .open_table(CHUNKS_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;

            let mut next_id = read_next_id(&meta)?;

            // Upsert host record.
            let host_record = make_host_record(host, &host_key, scanned_at);
            let host_bytes = encode_framed(&host_record);
            hosts
                .insert(host_key.as_str(), host_bytes.as_slice())
                .map_err(|e| classify_redb_error(&e.into()))?;

            // Ensure ancestor chain exists from filesystem root down to
            // `canonical_root`.
            let root_dir_id = ensure_ancestor_chain(
                &host_key,
                &canonical_root,
                &mut next_id,
                &mut path_to_dir,
                &mut dir_index,
                &mut dir_children,
            )?;

            // Remove old descendants and old chunk(s) under `root_dir_id` so
            // deleted directories never linger as ghost shards.
            purge_descendants(
                &host_key,
                root_dir_id,
                &mut path_to_dir,
                &mut dir_index,
                &mut dir_children,
                &mut chunks,
            )?;

            // Allocate deterministic dir_ids and chunk_ids for every shard
            // root in this arena.
            let mut node_to_dir_id: FxHashMap<NodeId, u64> =
                FxHashMap::default();
            let mut node_to_chunk_id: FxHashMap<NodeId, u64> =
                FxHashMap::default();
            node_to_dir_id.insert(arena.root(), root_dir_id);
            node_to_chunk_id.insert(arena.root(), alloc_id(&mut next_id));

            for &shard_node in &shard_nodes {
                if shard_node == arena.root() {
                    continue;
                }
                let shard_path = normalize_path(&arena.path_of(shard_node));
                let dir_id = if let Some(existing) = path_to_dir
                    .get((host_key.as_str(), shard_path.as_str()))
                    .map_err(|e| classify_redb_error(&e.into()))?
                    .map(|v| v.value())
                {
                    existing
                } else {
                    alloc_id(&mut next_id)
                };
                node_to_dir_id.insert(shard_node, dir_id);
                node_to_chunk_id.insert(shard_node, alloc_id(&mut next_id));
            }

            // Determine parent dir_id for each shard root (linking through
            // nearest ancestor shard root, creating intermediate stubs if
            // needed so whole-FS navigation works cleanly).
            let root_parent_id = dir_index
                .get((host_key.as_str(), root_dir_id))
                .map_err(|e| classify_redb_error(&e.into()))?
                .and_then(|v| decode_framed::<DirIndexEntry>(v.value()).ok())
                .map_or(0, |entry| entry.parent_id);

            let coverage = if options.max_depth.is_some() {
                Coverage::Shallow as u32
            } else {
                Coverage::Complete as u32
            };
            let max_depth_u32 = options
                .max_depth
                .map_or(0, |d| d.min(u32::MAX as usize) as u32);

            for &shard_node in &shard_nodes {
                let dir_id = node_to_dir_id[&shard_node];
                let chunk_id = node_to_chunk_id[&shard_node];
                let shard_path = normalize_path(&arena.path_of(shard_node));

                let parent_dir_id = if shard_node == arena.root() {
                    root_parent_id
                } else {
                    let parent_path = parent_normalized_path(&shard_path);
                    match parent_path {
                        Some(p) => ensure_ancestor_chain(
                            &host_key,
                            &p,
                            &mut next_id,
                            &mut path_to_dir,
                            &mut dir_index,
                            &mut dir_children,
                        )?,
                        None => 0,
                    }
                };

                // Encode chunk stopping at nested shard roots.
                let chunk = encode_subtree_chunk(
                    arena,
                    shard_node,
                    chunk_id,
                    dir_id,
                    scanned_at,
                    &node_to_dir_id,
                );
                let chunk_bytes = encode_framed(&chunk);
                chunks
                    .insert(
                        (host_key.as_str(), chunk_id),
                        chunk_bytes.as_slice(),
                    )
                    .map_err(|e| classify_redb_error(&e.into()))?;

                let node = arena.node(shard_node);
                let dir_ts = if node.scanned_at > 0 {
                    node.scanned_at
                } else {
                    scanned_at
                };
                let entry = DirIndexEntry {
                    dir_id,
                    parent_id: parent_dir_id,
                    name: leaf_name(&shard_path).to_owned(),
                    scanned_at_unix: dir_ts,
                    coverage,
                    scanned_max_depth: max_depth_u32,
                    options_mask: options.cache_topology_mask(),
                    mount_dev: 0,
                    dir_mtime: node.modified,
                    chunk_id,
                    allocated_bytes: node.allocated_bytes,
                    apparent_bytes: node.apparent_bytes,
                    files: node.files,
                    dirs: node.dirs,
                    newest_mtime: node.modified,
                };
                let entry_bytes = encode_framed(&entry);

                path_to_dir
                    .insert((host_key.as_str(), shard_path.as_str()), dir_id)
                    .map_err(|e| classify_redb_error(&e.into()))?;
                dir_index
                    .insert((host_key.as_str(), dir_id), entry_bytes.as_slice())
                    .map_err(|e| classify_redb_error(&e.into()))?;
                if parent_dir_id != 0 {
                    dir_children
                        .insert((host_key.as_str(), parent_dir_id, dir_id), ())
                        .map_err(|e| classify_redb_error(&e.into()))?;
                }
            }

            meta.insert("next_id", next_id.to_le_bytes().as_slice())
                .map_err(|e| classify_redb_error(&e.into()))?;
        }

        txn.commit().map_err(|e| classify_redb_error(&e.into()))?;
        Ok(())
    }

    #[cfg(not(feature = "cache-db"))]
    pub fn store_subtree(
        &self,
        _host: &HostKey,
        _arena: &TreeArena,
        _options: &ScanOptions,
        _scanned_at: u64,
    ) -> Result<(), FallbackReason> {
        Err(FallbackReason::DatabaseUnavailable {
            detail: "compiled without cache-db feature".to_owned(),
        })
    }

    /// Load a cached subtree rooted at `root_path` (supporting direct hits,
    /// narrowing into a cached ancestor shard, and partial ancestor hits).
    #[cfg(feature = "cache-db")]
    pub fn load_subtree(
        &self,
        host: &HostKey,
        root_path: &Path,
        options: &ScanOptions,
        now: u64,
        policy: &CachePolicy,
    ) -> Result<Option<CachedSubtree>, FallbackReason> {
        let host_key = host.as_db_key();
        let normalized = normalize_path(root_path);
        let ttl_secs = policy.ttl.as_secs();
        let max_stale_secs = policy.max_stale.map(|d| d.as_secs());

        let txn = self
            .db
            .begin_read()
            .map_err(|e| classify_redb_error(&e.into()))?;
        let path_to_dir = txn
            .open_table(PATH_TO_DIR_TABLE)
            .map_err(|e| classify_redb_error(&e.into()))?;
        let dir_index = txn
            .open_table(DIR_INDEX_TABLE)
            .map_err(|e| classify_redb_error(&e.into()))?;
        let dir_children = txn
            .open_table(DIR_CHILDREN_TABLE)
            .map_err(|e| classify_redb_error(&e.into()))?;
        let chunks = txn
            .open_table(CHUNKS_TABLE)
            .map_err(|e| classify_redb_error(&e.into()))?;

        // 1. Direct lookup on `normalized`.
        if let Some(dir_id) = path_to_dir
            .get((host_key.as_str(), normalized.as_str()))
            .map_err(|e| classify_redb_error(&e.into()))?
            .map(|v| v.value())
            && let Some(entry) = read_dir_entry(&dir_index, &host_key, dir_id)?
        {
            if entry.chunk_id != 0
                && is_entry_compatible(&entry, options)
                && !exceeds_max_stale(
                    entry.scanned_at_unix,
                    now,
                    max_stale_secs,
                )
            {
                let mut tracker = FreshnessTracker::default();
                let mut arena = load_shard_recursive(
                    &host_key,
                    &entry,
                    root_path.to_path_buf(),
                    now,
                    ttl_secs,
                    &dir_index,
                    &chunks,
                    &mut tracker,
                )?;
                arena
                    .aggregate(options.dedup_hardlinks, options.include_hidden);
                arena.sort_children(options.byte_metric());
                arena.refresh_statuses(now, ttl_secs);
                return Ok(Some(CachedSubtree {
                    arena,
                    freshness: tracker.finish(),
                }));
            }

            // Stub or invalidated directory that still has cached descendants
            // (e.g. scanned `~/src` earlier, now querying `~`).
            if entry.coverage == Coverage::Stub as u32
                || entry.scanned_at_unix == 0
            {
                let mut partial = TreeArena::new(
                    root_path.to_path_buf(),
                    leaf_name(&normalized),
                );
                partial
                    .node_mut(partial.root())
                    .set_status(SubtreeStatus::Unscanned);
                let mut cached_dirs = 0u32;
                let mut oldest: Option<u64> = None;
                let mut newest: Option<u64> = None;
                let grafted = graft_cached_children(
                    &host_key,
                    dir_id,
                    partial.root(),
                    &mut partial,
                    options,
                    now,
                    ttl_secs,
                    max_stale_secs,
                    &dir_index,
                    &dir_children,
                    &chunks,
                    &mut cached_dirs,
                    &mut oldest,
                    &mut newest,
                )?;
                if grafted > 0 {
                    partial.aggregate(
                        options.dedup_hardlinks,
                        options.include_hidden,
                    );
                    partial.sort_children(options.byte_metric());
                    partial.refresh_statuses(now, ttl_secs);
                    return Ok(Some(CachedSubtree {
                        arena: partial,
                        freshness: Freshness::Partial {
                            cached_dirs,
                            missing_dirs: 1,
                            oldest_at: oldest,
                            newest_at: newest,
                        },
                    }));
                }
            }
        }

        // 2. Narrowing lookup: walk up ancestors to find a cached shard that
        //    contains `normalized` inside its chunk.
        let mut cursor = normalized;
        let mut rel_parts: Vec<String> = Vec::new();
        while let Some(parent) = parent_normalized_path(&cursor) {
            rel_parts.push(leaf_name(&cursor).to_owned());
            if let Some(anc_id) = path_to_dir
                .get((host_key.as_str(), parent.as_str()))
                .map_err(|e| classify_redb_error(&e.into()))?
                .map(|v| v.value())
                && let Some(entry) =
                    read_dir_entry(&dir_index, &host_key, anc_id)?
                && entry.chunk_id != 0
                && is_entry_compatible(&entry, options)
                && !exceeds_max_stale(
                    entry.scanned_at_unix,
                    now,
                    max_stale_secs,
                )
            {
                let mut tracker = FreshnessTracker::default();
                let anc_arena = load_shard_recursive(
                    &host_key,
                    &entry,
                    PathBuf::from(&parent),
                    now,
                    ttl_secs,
                    &dir_index,
                    &chunks,
                    &mut tracker,
                )?;
                rel_parts.reverse();
                if let Some(target_node) = anc_arena
                    .find_relative(rel_parts.iter().map(String::as_str))
                {
                    let mut sub = anc_arena.extract_subtree(target_node);
                    sub.root_path = root_path.to_path_buf();
                    sub.aggregate(
                        options.dedup_hardlinks,
                        options.include_hidden,
                    );
                    sub.sort_children(options.byte_metric());
                    sub.refresh_statuses(now, ttl_secs);
                    return Ok(Some(CachedSubtree {
                        arena: sub,
                        freshness: tracker.finish(),
                    }));
                }
                // Found the containing shard and the path wasn't in it.
                return Ok(None);
            }
            cursor = parent;
        }

        Ok(None)
    }

    #[cfg(not(feature = "cache-db"))]
    pub const fn load_subtree(
        &self,
        _host: &HostKey,
        _root_path: &Path,
        _options: &ScanOptions,
        _now: u64,
        _policy: &CachePolicy,
    ) -> Result<Option<CachedSubtree>, FallbackReason> {
        Ok(None)
    }

    /// Preload the map of fresh cached shard paths strictly beneath `root_path`
    /// so a live walk can skip them with a lock-free O(1) lookup.
    #[cfg(feature = "cache-db")]
    pub fn fresh_subtrees_below(
        &self,
        host: &HostKey,
        root_path: &Path,
        options: &ScanOptions,
        now: u64,
        ttl_secs: u64,
        exclude: &FxHashSet<PathBuf>,
    ) -> Result<FxHashMap<PathBuf, u64>, FallbackReason> {
        let host_key = host.as_db_key();
        let normalized_root = normalize_path(root_path);
        let prefix = if normalized_root == "/" {
            "/".to_owned()
        } else {
            format!("{normalized_root}/")
        };

        let txn = self
            .db
            .begin_read()
            .map_err(|e| classify_redb_error(&e.into()))?;
        let path_to_dir = txn
            .open_table(PATH_TO_DIR_TABLE)
            .map_err(|e| classify_redb_error(&e.into()))?;
        let dir_index = txn
            .open_table(DIR_INDEX_TABLE)
            .map_err(|e| classify_redb_error(&e.into()))?;

        let mut fresh = FxHashMap::default();
        let start = (host_key.as_str(), prefix.as_str());
        let iter = path_to_dir
            .range(start..)
            .map_err(|e| classify_redb_error(&e.into()))?;

        for item in iter {
            let (key, val) =
                item.map_err(|e| classify_redb_error(&e.into()))?;
            let (entry_host, entry_path) = key.value();
            if entry_host != host_key || !entry_path.starts_with(&prefix) {
                break;
            }
            let dir_id = val.value();
            let path_buf = PathBuf::from(entry_path);
            if exclude
                .iter()
                .any(|ex| path_buf.starts_with(ex) || ex.starts_with(&path_buf))
            {
                continue;
            }
            if let Some(entry) = read_dir_entry(&dir_index, &host_key, dir_id)?
                && entry.chunk_id != 0
                && entry.scanned_at_unix > 0
                && now.saturating_sub(entry.scanned_at_unix) <= ttl_secs
                && is_entry_compatible(&entry, options)
            {
                fresh.insert(path_buf, dir_id);
            }
        }

        Ok(fresh)
    }

    #[cfg(not(feature = "cache-db"))]
    pub fn fresh_subtrees_below(
        &self,
        _host: &HostKey,
        _root_path: &Path,
        _options: &ScanOptions,
        _now: u64,
        _ttl_secs: u64,
        _exclude: &FxHashSet<PathBuf>,
    ) -> Result<FxHashMap<PathBuf, u64>, FallbackReason> {
        Ok(FxHashMap::default())
    }

    /// Load a single shard by `dir_id` (used when grafting a skipped fresh
    /// subtree during a wider walk).
    #[cfg(feature = "cache-db")]
    pub fn load_shard_by_dir_id(
        &self,
        host: &HostKey,
        dir_id: u64,
        root_path: PathBuf,
        now: u64,
        ttl_secs: u64,
    ) -> Result<Option<TreeArena>, FallbackReason> {
        let host_key = host.as_db_key();
        let txn = self
            .db
            .begin_read()
            .map_err(|e| classify_redb_error(&e.into()))?;
        let dir_index = txn
            .open_table(DIR_INDEX_TABLE)
            .map_err(|e| classify_redb_error(&e.into()))?;
        let chunks = txn
            .open_table(CHUNKS_TABLE)
            .map_err(|e| classify_redb_error(&e.into()))?;

        let Some(entry) = read_dir_entry(&dir_index, &host_key, dir_id)? else {
            return Ok(None);
        };
        if entry.chunk_id == 0 {
            return Ok(None);
        }
        let mut tracker = FreshnessTracker::default();
        let arena = load_shard_recursive(
            &host_key,
            &entry,
            root_path,
            now,
            ttl_secs,
            &dir_index,
            &chunks,
            &mut tracker,
        )?;
        Ok(Some(arena))
    }

    #[cfg(not(feature = "cache-db"))]
    pub fn load_shard_by_dir_id(
        &self,
        _host: &HostKey,
        _dir_id: u64,
        _root_path: PathBuf,
        _now: u64,
        _ttl_secs: u64,
    ) -> Result<Option<TreeArena>, FallbackReason> {
        Ok(None)
    }

    /// Mark one or more paths dirty in the cache (e.g. after files were
    /// deleted), invalidating the target subtrees and their ancestor rollups
    /// while preserving untouched fresh sibling shards.
    #[cfg(feature = "cache-db")]
    pub fn invalidate(
        &self,
        host: &HostKey,
        paths: &[PathBuf],
    ) -> Result<InvalidateReport, FallbackReason> {
        check_cache_headroom(&self.path, self.min_free_headroom)?;

        let host_key = host.as_db_key();
        let mut report = InvalidateReport::default();

        let txn = self
            .db
            .begin_write()
            .map_err(|e| classify_redb_error(&e.into()))?;
        {
            let mut path_to_dir = txn
                .open_table(PATH_TO_DIR_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;
            let mut dir_index = txn
                .open_table(DIR_INDEX_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;
            let mut dir_children = txn
                .open_table(DIR_CHILDREN_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;
            let mut chunks = txn
                .open_table(CHUNKS_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;

            for raw_path in paths {
                let normalized = normalize_potentially_deleted_path(raw_path);

                // Find either the exact directory or its nearest indexed
                // containing shard.
                let mut target_dir_id = path_to_dir
                    .get((host_key.as_str(), normalized.as_str()))
                    .map_err(|e| classify_redb_error(&e.into()))?
                    .map(|v| v.value());

                if target_dir_id.is_none() {
                    let mut cursor = normalized.clone();
                    while let Some(parent) = parent_normalized_path(&cursor) {
                        if let Some(id) = path_to_dir
                            .get((host_key.as_str(), parent.as_str()))
                            .map_err(|e| classify_redb_error(&e.into()))?
                            .map(|v| v.value())
                        {
                            target_dir_id = Some(id);
                            break;
                        }
                        cursor = parent;
                    }
                }

                let Some(dir_id) = target_dir_id else {
                    continue;
                };

                // Purge cached descendants strictly beneath `dir_id` if `dir_id`
                // matches `normalized` exactly (if we fell back to a parent
                // shard, we only dirty that parent shard without wiping
                // unrelated indexed child shards).
                let exact_match = path_to_dir
                    .get((host_key.as_str(), normalized.as_str()))
                    .map_err(|e| classify_redb_error(&e.into()))?
                    .is_some();

                if exact_match {
                    let removed = purge_descendants(
                        &host_key,
                        dir_id,
                        &mut path_to_dir,
                        &mut dir_index,
                        &mut dir_children,
                        &mut chunks,
                    )?;
                    report.invalidated_dirs += removed;
                }

                // Mark the target entry itself dirty (timestamp = 0).
                if let Some(mut entry) =
                    read_dir_entry(&dir_index, &host_key, dir_id)?
                {
                    if entry.chunk_id != 0 {
                        let _ = chunks
                            .remove((host_key.as_str(), entry.chunk_id))
                            .map_err(|e| classify_redb_error(&e.into()))?;
                        entry.chunk_id = 0;
                    }
                    entry.scanned_at_unix = 0;
                    entry.coverage = Coverage::Stub as u32;
                    let parent_id = entry.parent_id;
                    let bytes = encode_framed(&entry);
                    dir_index
                        .insert((host_key.as_str(), dir_id), bytes.as_slice())
                        .map_err(|e| classify_redb_error(&e.into()))?;
                    report.invalidated_dirs += 1;

                    // Mark ancestors stale so a wider scan doesn't skip the
                    // parent while keeping untouched sibling shards fresh.
                    let mut anc_id = parent_id;
                    while anc_id != 0 {
                        let Some(mut anc) =
                            read_dir_entry(&dir_index, &host_key, anc_id)?
                        else {
                            break;
                        };
                        let next_parent = anc.parent_id;
                        if anc.scanned_at_unix != 0 {
                            anc.scanned_at_unix = 0;
                            let bytes = encode_framed(&anc);
                            dir_index
                                .insert(
                                    (host_key.as_str(), anc_id),
                                    bytes.as_slice(),
                                )
                                .map_err(|e| classify_redb_error(&e.into()))?;
                            report.touched_ancestors += 1;
                        }
                        anc_id = next_parent;
                    }
                }
            }
        }
        txn.commit().map_err(|e| classify_redb_error(&e.into()))?;
        Ok(report)
    }

    #[cfg(not(feature = "cache-db"))]
    pub fn invalidate(
        &self,
        _host: &HostKey,
        _paths: &[PathBuf],
    ) -> Result<InvalidateReport, FallbackReason> {
        Ok(InvalidateReport::default())
    }

    /// Return cache statistics (file size, indexed dirs, chunk count).
    #[cfg(feature = "cache-db")]
    pub fn stats(&self) -> Result<CacheStats, FallbackReason> {
        let db_bytes = std::fs::metadata(&self.path).map_or(0, |m| m.len());
        let txn = self
            .db
            .begin_read()
            .map_err(|e| classify_redb_error(&e.into()))?;
        let hosts = txn
            .open_table(HOSTS_TABLE)
            .map_err(|e| classify_redb_error(&e.into()))?
            .len()
            .map_err(|e| classify_redb_error(&e.into()))?;
        let indexed_dirs = txn
            .open_table(DIR_INDEX_TABLE)
            .map_err(|e| classify_redb_error(&e.into()))?
            .len()
            .map_err(|e| classify_redb_error(&e.into()))?;
        let chunks = txn
            .open_table(CHUNKS_TABLE)
            .map_err(|e| classify_redb_error(&e.into()))?
            .len()
            .map_err(|e| classify_redb_error(&e.into()))?;
        Ok(CacheStats {
            db_path: Some(self.path.clone()),
            db_bytes,
            hosts,
            indexed_dirs,
            chunks,
        })
    }

    #[cfg(not(feature = "cache-db"))]
    pub fn stats(&self) -> Result<CacheStats, FallbackReason> {
        Ok(CacheStats::default())
    }

    /// Delete every cached host, directory index entry, and chunk.
    #[cfg(feature = "cache-db")]
    pub fn clear(&self) -> Result<(), FallbackReason> {
        let txn = self
            .db
            .begin_write()
            .map_err(|e| classify_redb_error(&e.into()))?;
        {
            for table_fn in [
                clear_str_bytes_table(&txn, HOSTS_TABLE),
                clear_path_table(&txn),
                clear_dir_index_table(&txn),
                clear_dir_children_table(&txn),
                clear_chunks_table(&txn),
            ] {
                table_fn?;
            }
        }
        txn.commit().map_err(|e| classify_redb_error(&e.into()))?;
        Ok(())
    }

    #[cfg(not(feature = "cache-db"))]
    pub const fn clear(&self) -> Result<(), FallbackReason> {
        Ok(())
    }

    /// Prune cached chunks older than `max_age` seconds.
    #[cfg(feature = "cache-db")]
    pub fn prune_older_than(
        &self,
        now: u64,
        max_age_secs: u64,
    ) -> Result<u64, FallbackReason> {
        let mut pruned = 0u64;
        let txn = self
            .db
            .begin_write()
            .map_err(|e| classify_redb_error(&e.into()))?;
        {
            let mut dir_index = txn
                .open_table(DIR_INDEX_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;
            let mut chunks = txn
                .open_table(CHUNKS_TABLE)
                .map_err(|e| classify_redb_error(&e.into()))?;

            let mut expired: Vec<(String, u64, DirIndexEntry)> = Vec::new();
            {
                let iter = dir_index
                    .iter()
                    .map_err(|e| classify_redb_error(&e.into()))?;
                for item in iter {
                    let (key, val) =
                        item.map_err(|e| classify_redb_error(&e.into()))?;
                    let (host, dir_id) = key.value();
                    let Ok(entry) = decode_framed::<DirIndexEntry>(val.value())
                    else {
                        continue;
                    };
                    if entry.chunk_id != 0
                        && now.saturating_sub(entry.scanned_at_unix)
                            > max_age_secs
                    {
                        expired.push((host.to_owned(), dir_id, entry));
                    }
                }
            }

            for (host, dir_id, mut entry) in expired {
                let _ = chunks
                    .remove((host.as_str(), entry.chunk_id))
                    .map_err(|e| classify_redb_error(&e.into()))?;
                entry.chunk_id = 0;
                entry.scanned_at_unix = 0;
                entry.coverage = Coverage::Stub as u32;
                let bytes = encode_framed(&entry);
                dir_index
                    .insert((host.as_str(), dir_id), bytes.as_slice())
                    .map_err(|e| classify_redb_error(&e.into()))?;
                pruned += 1;
            }
        }
        txn.commit().map_err(|e| classify_redb_error(&e.into()))?;
        Ok(pruned)
    }

    #[cfg(not(feature = "cache-db"))]
    pub const fn prune_older_than(
        &self,
        _now: u64,
        _max_age_secs: u64,
    ) -> Result<u64, FallbackReason> {
        Ok(0)
    }
}

/// Choose which directories in `arena` become independent shards.
#[cfg(feature = "cache-db")]
fn choose_shard_roots(arena: &TreeArena, counts: &[u32]) -> Vec<NodeId> {
    let mut shards = vec![arena.root()];
    let mut stack: Vec<(NodeId, usize)> =
        arena.children(arena.root()).map(|kid| (kid, 1)).collect();

    while let Some((node_id, depth)) = stack.pop() {
        let node = arena.node(node_id);
        if !node.is_dir() {
            continue;
        }
        let count = counts[node_id.index()];
        let is_shallow_shard =
            depth <= SHALLOW_SHARD_DEPTH && count >= SHALLOW_SHARD_MIN_NODES;
        let is_large_shard = count >= SHARD_NODE_THRESHOLD;
        if is_shallow_shard || is_large_shard {
            shards.push(node_id);
        }
        for child in arena.children(node_id) {
            if arena.node(child).is_dir() {
                stack.push((child, depth + 1));
            }
        }
    }

    shards
}

#[cfg(feature = "cache-db")]
#[derive(Default)]
struct FreshnessTracker {
    oldest: Option<u64>,
    newest: Option<u64>,
    stale_dirs: u32,
}

#[cfg(feature = "cache-db")]
impl FreshnessTracker {
    fn observe(&mut self, scanned_at: u64, now: u64, ttl_secs: u64) {
        self.oldest = Some(match self.oldest {
            Some(old) => old.min(scanned_at),
            None => scanned_at,
        });
        self.newest = Some(match self.newest {
            Some(new) => new.max(scanned_at),
            None => scanned_at,
        });
        if scanned_at == 0 || now.saturating_sub(scanned_at) > ttl_secs {
            self.stale_dirs = self.stale_dirs.saturating_add(1);
        }
    }

    fn finish(self) -> Freshness {
        let oldest_at = self.oldest.unwrap_or(0);
        let newest_at = self.newest.unwrap_or(0);
        if self.stale_dirs == 0 && oldest_at > 0 {
            Freshness::Fresh {
                oldest_at,
                newest_at,
            }
        } else {
            Freshness::Stale {
                oldest_at,
                newest_at,
                stale_dirs: self.stale_dirs.max(1),
            }
        }
    }
}

#[cfg(feature = "cache-db")]
#[allow(clippy::too_many_arguments, reason = "recursive DB loader state")]
fn load_shard_recursive(
    host_key: &str,
    entry: &DirIndexEntry,
    root_path: PathBuf,
    now: u64,
    ttl_secs: u64,
    dir_index: &redb::ReadOnlyTable<(&str, u64), &[u8]>,
    chunks: &redb::ReadOnlyTable<(&str, u64), &[u8]>,
    tracker: &mut FreshnessTracker,
) -> Result<TreeArena, FallbackReason> {
    let raw = chunks
        .get((host_key, entry.chunk_id))
        .map_err(|e| classify_redb_error(&e.into()))?
        .ok_or_else(|| FallbackReason::DatabaseUnavailable {
            detail: format!("missing chunk {}", entry.chunk_id),
        })?;
    let proto_chunk: SubtreeChunk =
        decode_framed(raw.value()).map_err(|e| {
            FallbackReason::DatabaseUnavailable {
                detail: format!("corrupt chunk {}: {e}", entry.chunk_id),
            }
        })?;
    let mut decoded =
        decode_subtree_chunk(&proto_chunk, root_path).map_err(|e| {
            FallbackReason::DatabaseUnavailable {
                detail: format!("invalid chunk {}: {e}", entry.chunk_id),
            }
        })?;

    tracker.observe(entry.scanned_at_unix, now, ttl_secs);
    decoded.arena.node_mut(decoded.arena.root()).scanned_at =
        entry.scanned_at_unix;

    for (stub_node_id, ext_dir_id) in decoded.external_links {
        let Some(child_entry) =
            read_dir_entry(dir_index, host_key, ext_dir_id)?
        else {
            continue;
        };
        if child_entry.chunk_id == 0 {
            decoded
                .arena
                .node_mut(stub_node_id)
                .set_status(SubtreeStatus::Unscanned);
            tracker.observe(0, now, ttl_secs);
            continue;
        }
        let child_path = decoded.arena.path_of(stub_node_id);
        let child_arena = load_shard_recursive(
            host_key,
            &child_entry,
            child_path,
            now,
            ttl_secs,
            dir_index,
            chunks,
            tracker,
        )?;
        decoded
            .arena
            .replace_node_contents(stub_node_id, &child_arena);
    }

    Ok(decoded.arena)
}

#[cfg(feature = "cache-db")]
#[allow(clippy::too_many_arguments, reason = "recursive partial graft state")]
fn graft_cached_children(
    host_key: &str,
    parent_dir_id: u64,
    parent_node: NodeId,
    target: &mut TreeArena,
    options: &ScanOptions,
    now: u64,
    ttl_secs: u64,
    max_stale_secs: Option<u64>,
    dir_index: &redb::ReadOnlyTable<(&str, u64), &[u8]>,
    dir_children: &redb::ReadOnlyTable<(&str, u64, u64), ()>,
    chunks: &redb::ReadOnlyTable<(&str, u64), &[u8]>,
    cached_dirs: &mut u32,
    oldest: &mut Option<u64>,
    newest: &mut Option<u64>,
) -> Result<u32, FallbackReason> {
    let mut grafted = 0u32;
    let range_start = (host_key, parent_dir_id, 0u64);
    let range_end = (host_key, parent_dir_id, u64::MAX);
    let iter = dir_children
        .range(range_start..=range_end)
        .map_err(|e| classify_redb_error(&e.into()))?;

    let mut child_ids = Vec::new();
    for item in iter {
        let (key, _) = item.map_err(|e| classify_redb_error(&e.into()))?;
        let (h, p, cid) = key.value();
        if h == host_key && p == parent_dir_id {
            child_ids.push(cid);
        }
    }

    for child_id in child_ids {
        let Some(entry) = read_dir_entry(dir_index, host_key, child_id)? else {
            continue;
        };
        if entry.chunk_id != 0
            && is_entry_compatible(&entry, options)
            && !exceeds_max_stale(entry.scanned_at_unix, now, max_stale_secs)
        {
            let mut tracker = FreshnessTracker::default();
            let child_path = target.path_of(parent_node).join(&entry.name);
            let child_arena = load_shard_recursive(
                host_key,
                &entry,
                child_path,
                now,
                ttl_secs,
                dir_index,
                chunks,
                &mut tracker,
            )?;
            target.graft_subtree(parent_node, &child_arena, Some(&entry.name));
            grafted += 1;
            *cached_dirs = cached_dirs.saturating_add(1);
            if let Some(o) = tracker.oldest {
                *oldest = Some(oldest.map_or(o, |cur| cur.min(o)));
            }
            if let Some(n) = tracker.newest {
                *newest = Some(newest.map_or(n, |cur| cur.max(n)));
            }
        } else {
            // Recurse through intermediate stub directories.
            let stub_id = target.add_child(
                parent_node,
                &entry.name,
                crate::types::NodeKind::Directory,
                0,
                0,
                0,
                crate::tree::NodeFlags::new(crate::types::NodeKind::Directory),
            );
            target
                .node_mut(stub_id)
                .set_status(SubtreeStatus::Unscanned);
            let sub = graft_cached_children(
                host_key,
                child_id,
                stub_id,
                target,
                options,
                now,
                ttl_secs,
                max_stale_secs,
                dir_index,
                dir_children,
                chunks,
                cached_dirs,
                oldest,
                newest,
            )?;
            grafted += sub;
        }
    }

    Ok(grafted)
}

#[cfg(feature = "cache-db")]
fn read_dir_entry<T>(
    table: &T,
    host_key: &str,
    dir_id: u64,
) -> Result<Option<DirIndexEntry>, FallbackReason>
where
    T: redb::ReadableTable<(&'static str, u64), &'static [u8]>,
{
    let Some(bytes) = table
        .get((host_key, dir_id))
        .map_err(|e| classify_redb_error(&e.into()))?
    else {
        return Ok(None);
    };
    let entry: DirIndexEntry = decode_framed(bytes.value()).map_err(|e| {
        FallbackReason::DatabaseUnavailable {
            detail: format!("corrupt dir_index entry {dir_id}: {e}"),
        }
    })?;
    Ok(Some(entry))
}

#[cfg(feature = "cache-db")]
fn ensure_ancestor_chain(
    host_key: &str,
    normalized_path: &str,
    next_id: &mut u64,
    path_to_dir: &mut redb::Table<'_, (&str, &str), u64>,
    dir_index: &mut redb::Table<'_, (&str, u64), &[u8]>,
    dir_children: &mut redb::Table<'_, (&str, u64, u64), ()>,
) -> Result<u64, FallbackReason> {
    let ancestors = ancestor_paths(normalized_path);
    let mut parent_id = 0u64;

    for anc in ancestors {
        let dir_id = if let Some(existing) = path_to_dir
            .get((host_key, anc.as_str()))
            .map_err(|e| classify_redb_error(&e.into()))?
            .map(|v| v.value())
        {
            existing
        } else {
            let id = alloc_id(next_id);
            let stub = DirIndexEntry {
                dir_id: id,
                parent_id,
                name: leaf_name(&anc).to_owned(),
                scanned_at_unix: 0,
                coverage: Coverage::Stub as u32,
                scanned_max_depth: 0,
                options_mask: 0,
                mount_dev: 0,
                dir_mtime: 0,
                chunk_id: 0,
                allocated_bytes: 0,
                apparent_bytes: 0,
                files: 0,
                dirs: 0,
                newest_mtime: 0,
            };
            let bytes = encode_framed(&stub);
            path_to_dir
                .insert((host_key, anc.as_str()), id)
                .map_err(|e| classify_redb_error(&e.into()))?;
            dir_index
                .insert((host_key, id), bytes.as_slice())
                .map_err(|e| classify_redb_error(&e.into()))?;
            if parent_id != 0 {
                dir_children
                    .insert((host_key, parent_id, id), ())
                    .map_err(|e| classify_redb_error(&e.into()))?;
            }
            id
        };
        parent_id = dir_id;
    }

    Ok(parent_id)
}

#[cfg(feature = "cache-db")]
fn purge_descendants(
    host_key: &str,
    root_dir_id: u64,
    path_to_dir: &mut redb::Table<'_, (&str, &str), u64>,
    dir_index: &mut redb::Table<'_, (&str, u64), &[u8]>,
    dir_children: &mut redb::Table<'_, (&str, u64, u64), ()>,
    chunks: &mut redb::Table<'_, (&str, u64), &[u8]>,
) -> Result<u64, FallbackReason> {
    // Also drop old chunk on `root_dir_id` itself before replacing it.
    if let Some(root_entry) = read_dir_entry(dir_index, host_key, root_dir_id)?
        && root_entry.chunk_id != 0
    {
        let _ = chunks
            .remove((host_key, root_entry.chunk_id))
            .map_err(|e| classify_redb_error(&e.into()))?;
    }

    let mut stack = vec![root_dir_id];
    let mut edges_to_remove = Vec::new();
    let mut dirs_to_remove = Vec::new();

    while let Some(parent) = stack.pop() {
        let range_start = (host_key, parent, 0u64);
        let range_end = (host_key, parent, u64::MAX);
        let mut kids = Vec::new();
        {
            let iter = dir_children
                .range(range_start..=range_end)
                .map_err(|e| classify_redb_error(&e.into()))?;
            for item in iter {
                let (key, _) =
                    item.map_err(|e| classify_redb_error(&e.into()))?;
                let (h, p, cid) = key.value();
                if h == host_key && p == parent {
                    kids.push(cid);
                }
            }
        }
        for cid in kids {
            edges_to_remove.push((parent, cid));
            dirs_to_remove.push(cid);
            stack.push(cid);
        }
    }

    for (p, cid) in edges_to_remove {
        let _ = dir_children
            .remove((host_key, p, cid))
            .map_err(|e| classify_redb_error(&e.into()))?;
    }

    let remove_set: FxHashSet<u64> = dirs_to_remove.iter().copied().collect();
    let mut removed_count = 0u64;
    for cid in dirs_to_remove {
        if let Some(entry) = read_dir_entry(dir_index, host_key, cid)?
            && entry.chunk_id != 0
        {
            let _ = chunks
                .remove((host_key, entry.chunk_id))
                .map_err(|e| classify_redb_error(&e.into()))?;
        }
        let _ = dir_index
            .remove((host_key, cid))
            .map_err(|e| classify_redb_error(&e.into()))?;
        removed_count += 1;
    }

    // Clean stale `path_to_dir` entries pointing to removed dir_ids.
    if !remove_set.is_empty() {
        let mut stale_paths = Vec::new();
        {
            let iter = path_to_dir
                .range((host_key, "")..)
                .map_err(|e| classify_redb_error(&e.into()))?;
            for item in iter {
                let (key, val) =
                    item.map_err(|e| classify_redb_error(&e.into()))?;
                let (h, p) = key.value();
                if h != host_key {
                    break;
                }
                if remove_set.contains(&val.value()) {
                    stale_paths.push(p.to_owned());
                }
            }
        }
        for p in stale_paths {
            let _ = path_to_dir
                .remove((host_key, p.as_str()))
                .map_err(|e| classify_redb_error(&e.into()))?;
        }
    }

    Ok(removed_count)
}

#[cfg(feature = "cache-db")]
fn read_next_id(
    meta: &redb::Table<'_, &str, &[u8]>,
) -> Result<u64, FallbackReason> {
    let val = meta
        .get("next_id")
        .map_err(|e| classify_redb_error(&e.into()))?;
    match val {
        Some(bytes) if bytes.value().len() == 8 => {
            let mut arr = [0u8; 8];
            arr.copy_from_slice(bytes.value());
            Ok(u64::from_le_bytes(arr))
        }
        _ => Ok(1),
    }
}

#[cfg(feature = "cache-db")]
const fn alloc_id(next_id: &mut u64) -> u64 {
    let id = *next_id;
    *next_id = next_id.saturating_add(1);
    id
}

#[cfg(feature = "cache-db")]
const fn is_entry_compatible(
    entry: &DirIndexEntry,
    options: &ScanOptions,
) -> bool {
    if entry.options_mask != options.cache_topology_mask() {
        return false;
    }
    match Coverage::from_u32(entry.coverage) {
        Coverage::Complete => true,
        Coverage::Shallow => match options.max_depth {
            Some(req) => (entry.scanned_max_depth as usize) >= req,
            None => false,
        },
        _ => false,
    }
}

#[cfg(feature = "cache-db")]
const fn exceeds_max_stale(
    scanned_at: u64,
    now: u64,
    max_stale_secs: Option<u64>,
) -> bool {
    match max_stale_secs {
        Some(limit) => {
            scanned_at == 0 || now.saturating_sub(scanned_at) > limit
        }
        None => false,
    }
}

#[cfg(feature = "cache-db")]
fn make_host_record(
    host: &HostKey,
    host_key: &str,
    updated_at_unix: u64,
) -> HostRecord {
    match host {
        HostKey::Local => HostRecord {
            host_key: host_key.to_owned(),
            hostname: crate::types::HostIdentity::current()
                .map_or_else(|| "local".to_owned(), |id| id.hostname),
            machine_id: crate::types::HostIdentity::current()
                .and_then(|id| id.machine_id)
                .unwrap_or_default(),
            platform: std::env::consts::OS.to_owned(),
            path_style: u32::from(cfg!(windows)),
            updated_at_unix,
        },
        HostKey::Remote(identity) => HostRecord {
            host_key: host_key.to_owned(),
            hostname: identity.hostname.clone(),
            machine_id: identity.machine_id.clone().unwrap_or_default(),
            platform: "unix".to_owned(),
            path_style: 0,
            updated_at_unix,
        },
    }
}

/// Normalize an existing or canonical path into a clean string key.
pub fn normalize_path(path: &Path) -> String {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    normalize_path_lexical(&resolved)
}

/// Normalize a path that may already have been deleted by canonicalizing its
/// nearest existing ancestor and re-appending the remaining components.
pub fn normalize_potentially_deleted_path(path: &Path) -> String {
    if let Ok(canon) = path.canonicalize() {
        return normalize_path_lexical(&canon);
    }
    let mut existing = path.to_path_buf();
    let mut tail = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            break;
        };
        tail.push(name.to_os_string());
        if !existing.pop() {
            break;
        }
    }
    let mut base = existing.canonicalize().unwrap_or(existing);
    for part in tail.into_iter().rev() {
        base.push(part);
    }
    normalize_path_lexical(&base)
}

fn normalize_path_lexical(path: &Path) -> String {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    let text = out.to_string_lossy();
    if text.len() > 1 && text.ends_with('/') {
        text.trim_end_matches('/').to_owned()
    } else if text.is_empty() {
        "/".to_owned()
    } else {
        text.into_owned()
    }
}

#[cfg(feature = "cache-db")]
fn parent_normalized_path(path: &str) -> Option<String> {
    if path == "/" || path.is_empty() {
        return None;
    }
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_owned()),
        Some(idx) => Some(trimmed[..idx].to_owned()),
        None => None,
    }
}

#[cfg(feature = "cache-db")]
fn ancestor_paths(path: &str) -> Vec<String> {
    let mut chain = vec![path.to_owned()];
    let mut cur = path.to_owned();
    while let Some(parent) = parent_normalized_path(&cur) {
        chain.push(parent.clone());
        cur = parent;
    }
    chain.reverse();
    chain
}

#[cfg(feature = "cache-db")]
fn leaf_name(path: &str) -> &str {
    if path == "/" {
        return "/";
    }
    path.rsplit('/').next().unwrap_or(path)
}

/// Classify a `std::io::Error` into a [`FallbackReason`].
pub fn classify_io_error(error: &io::Error) -> FallbackReason {
    match error.kind() {
        io::ErrorKind::StorageFull => FallbackReason::DiskFull,
        io::ErrorKind::ReadOnlyFilesystem => FallbackReason::ReadOnlyFilesystem,
        io::ErrorKind::PermissionDenied => FallbackReason::PermissionDenied,
        _ => {
            // Check raw Unix errno for ENOSPC (28) / EDQUOT (122) / EROFS (30)
            // when not mapped to a dedicated ErrorKind.
            #[cfg(unix)]
            if let Some(code) = error.raw_os_error() {
                if code == 28 || code == 122 {
                    return FallbackReason::DiskFull;
                }
                if code == 30 {
                    return FallbackReason::ReadOnlyFilesystem;
                }
            }
            FallbackReason::DatabaseUnavailable {
                detail: error.to_string(),
            }
        }
    }
}

#[cfg(feature = "cache-db")]
fn classify_redb_error(error: &redb::Error) -> FallbackReason {
    match error {
        redb::Error::Io(io_err) => classify_io_error(io_err),
        other => FallbackReason::DatabaseUnavailable {
            detail: other.to_string(),
        },
    }
}

#[cfg(feature = "cache-db")]
fn clear_str_bytes_table(
    txn: &redb::WriteTransaction,
    def: TableDefinition<'_, &str, &[u8]>,
) -> Result<(), FallbackReason> {
    let mut table = txn
        .open_table(def)
        .map_err(|e| classify_redb_error(&e.into()))?;
    let keys: Vec<String> = table
        .iter()
        .map_err(|e| classify_redb_error(&e.into()))?
        .filter_map(Result::ok)
        .map(|(k, _)| k.value().to_owned())
        .collect();
    for k in keys {
        let _ = table
            .remove(k.as_str())
            .map_err(|e| classify_redb_error(&e.into()))?;
    }
    Ok(())
}

#[cfg(feature = "cache-db")]
fn clear_path_table(
    txn: &redb::WriteTransaction,
) -> Result<(), FallbackReason> {
    let mut table = txn
        .open_table(PATH_TO_DIR_TABLE)
        .map_err(|e| classify_redb_error(&e.into()))?;
    let keys: Vec<(String, String)> = table
        .iter()
        .map_err(|e| classify_redb_error(&e.into()))?
        .filter_map(Result::ok)
        .map(|(k, _)| (k.value().0.to_owned(), k.value().1.to_owned()))
        .collect();
    for (h, p) in keys {
        let _ = table
            .remove((h.as_str(), p.as_str()))
            .map_err(|e| classify_redb_error(&e.into()))?;
    }
    Ok(())
}

#[cfg(feature = "cache-db")]
fn clear_dir_index_table(
    txn: &redb::WriteTransaction,
) -> Result<(), FallbackReason> {
    let mut table = txn
        .open_table(DIR_INDEX_TABLE)
        .map_err(|e| classify_redb_error(&e.into()))?;
    let keys: Vec<(String, u64)> = table
        .iter()
        .map_err(|e| classify_redb_error(&e.into()))?
        .filter_map(Result::ok)
        .map(|(k, _)| (k.value().0.to_owned(), k.value().1))
        .collect();
    for (h, id) in keys {
        let _ = table
            .remove((h.as_str(), id))
            .map_err(|e| classify_redb_error(&e.into()))?;
    }
    Ok(())
}

#[cfg(feature = "cache-db")]
fn clear_dir_children_table(
    txn: &redb::WriteTransaction,
) -> Result<(), FallbackReason> {
    let mut table = txn
        .open_table(DIR_CHILDREN_TABLE)
        .map_err(|e| classify_redb_error(&e.into()))?;
    let keys: Vec<(String, u64, u64)> = table
        .iter()
        .map_err(|e| classify_redb_error(&e.into()))?
        .filter_map(Result::ok)
        .map(|(k, _)| (k.value().0.to_owned(), k.value().1, k.value().2))
        .collect();
    for (h, p, c) in keys {
        let _ = table
            .remove((h.as_str(), p, c))
            .map_err(|e| classify_redb_error(&e.into()))?;
    }
    Ok(())
}

#[cfg(feature = "cache-db")]
fn clear_chunks_table(
    txn: &redb::WriteTransaction,
) -> Result<(), FallbackReason> {
    let mut table = txn
        .open_table(CHUNKS_TABLE)
        .map_err(|e| classify_redb_error(&e.into()))?;
    let keys: Vec<(String, u64)> = table
        .iter()
        .map_err(|e| classify_redb_error(&e.into()))?
        .filter_map(Result::ok)
        .map(|(k, _)| (k.value().0.to_owned(), k.value().1))
        .collect();
    for (h, id) in keys {
        let _ = table
            .remove((h.as_str(), id))
            .map_err(|e| classify_redb_error(&e.into()))?;
    }
    Ok(())
}

#[cfg(all(test, feature = "cache-db"))]
mod tests {
    use super::*;
    use crate::tree::NodeFlags;
    use crate::types::NodeKind;

    #[test]
    fn store_narrow_widen_and_invalidate_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = CacheDb::open(dir.path().join("cache.redb"), 0, false)
            .expect("open db");

        // Build a synthetic arena for `/fake/home/src`.
        let mut src = TreeArena::new(PathBuf::from("/fake/home/src"), "src");
        let app = src.add_child(
            src.root(),
            "app",
            NodeKind::Directory,
            0,
            0,
            100,
            NodeFlags::new(NodeKind::Directory),
        );
        src.add_child(
            app,
            "main.rs",
            NodeKind::File,
            4096,
            250,
            100,
            NodeFlags::new(NodeKind::File),
        );
        src.aggregate(true, true);

        let opts = ScanOptions::default();
        let policy = CachePolicy::default();
        db.store_subtree(&HostKey::Local, &src, &opts, 1_000)
            .expect("store");

        // 1. Direct fresh hit at `/fake/home/src`.
        let hit = db
            .load_subtree(
                &HostKey::Local,
                Path::new("/fake/home/src"),
                &opts,
                1_100,
                &policy,
            )
            .expect("load")
            .expect("hit");
        assert!(matches!(hit.freshness, Freshness::Fresh { .. }));
        assert_eq!(hit.arena.node(hit.arena.root()).allocated_bytes, 4096);
        assert_eq!(hit.arena.node(hit.arena.root()).apparent_bytes, 250);

        // 2. Narrowing hit into `/fake/home/src/app` packed inside the chunk!
        let narrow = db
            .load_subtree(
                &HostKey::Local,
                Path::new("/fake/home/src/app"),
                &opts,
                1_100,
                &policy,
            )
            .expect("narrow")
            .expect("narrow hit");
        assert!(matches!(narrow.freshness, Freshness::Fresh { .. }));
        assert_eq!(narrow.arena.node(narrow.arena.root()).apparent_bytes, 250);

        // 3. Partial hit at ancestor `/fake/home` (grafts cached `src` child).
        let partial = db
            .load_subtree(
                &HostKey::Local,
                Path::new("/fake/home"),
                &opts,
                1_100,
                &policy,
            )
            .expect("partial")
            .expect("partial hit");
        assert!(matches!(partial.freshness, Freshness::Partial { .. }));
        assert_eq!(
            partial.arena.node(partial.arena.root()).allocated_bytes,
            4096
        );

        // 4. Stale detection past 15-minute TTL (1000 + 1000 > 900s).
        let stale = db
            .load_subtree(
                &HostKey::Local,
                Path::new("/fake/home/src"),
                &opts,
                2_000,
                &policy,
            )
            .expect("stale")
            .expect("stale hit");
        assert!(matches!(stale.freshness, Freshness::Stale { .. }));

        // 5. Targeted invalidation clears `/fake/home/src`.
        let report = db
            .invalidate(&HostKey::Local, &[PathBuf::from("/fake/home/src")])
            .expect("invalidate");
        assert!(report.invalidated_dirs >= 1);
        let after = db
            .load_subtree(
                &HostKey::Local,
                Path::new("/fake/home/src"),
                &opts,
                1_100,
                &policy,
            )
            .expect("after");
        assert!(after.is_none());
    }
}

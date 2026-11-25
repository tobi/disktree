//! High-level [`Scanner`] and priority-interruptible SWR [`ScanSession`].
//!
//! Coordinates the cache database, automatic fallback to in-memory/streaming,
//! stale-while-revalidate snapshots, fresh-subtree skipping, and two-lane
//! `Immediate` / `Background` priority preemption.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::db::{CacheDb, InvalidateReport};
use crate::mounts::default_cache_db_path;
use crate::stream::{StreamItem, stream_walk};
use crate::tree::TreeArena;
use crate::types::{
    ActiveStorageMode, CachePolicy, CacheStats, FallbackReason, Freshness,
    HostKey, Priority, ScanOptions, ScanStats, StorageMode, unix_now,
};
use crate::walk::{
    DirStartHook, DirTallyHook, MapSkipOracle, PauseGate, ShardLoader,
    SkipOracle, WalkConfig, WalkProgress, walk_parallel,
};

/// A tree snapshot emitted by a [`ScanSession`].
#[derive(Clone, Debug)]
pub struct ScanSnapshot {
    pub arena: Arc<TreeArena>,
    pub freshness: Freshness,
    pub stats: ScanStats,
    pub storage_mode: ActiveStorageMode,
    pub fallback_reason: Option<FallbackReason>,
}

/// Incremental events emitted by a [`ScanSession`].
#[derive(Clone, Debug)]
pub enum ScanEvent {
    /// Either an immediate cached (`Fresh`/`Stale`/`Partial`) snapshot or the
    /// final `Live` snapshot once revalidation finishes.
    Snapshot(ScanSnapshot),
    /// A directory has started actively scanning (UI should show `Updating`).
    SubtreeUpdating { path: PathBuf, priority: Priority },
    /// A targeted/prioritized subtree finished scanning and committed fresh
    /// data; UI can splice and paint it `Fresh` immediately.
    SubtreeRefreshed {
        path: PathBuf,
        arena: Arc<TreeArena>,
        scanned_at: u64,
        priority: Priority,
    },
    /// Periodic progress counters during a live walk.
    Progress {
        stats: ScanStats,
        active_path: Option<PathBuf>,
    },
    /// Storage downgraded automatically (e.g. disk full or read-only).
    StorageFallback {
        from: ActiveStorageMode,
        to: ActiveStorageMode,
        reason: FallbackReason,
    },
    /// Non-fatal scan error.
    Error { message: String },
    /// Background revalidation completed (or cache hit needed no walk).
    Finished,
}

/// Pure state machine for [`Scanner`]'s storage backend transitions.
///
/// Atomic transition: `step(state, event) -> (state, Vec<Effect>)`.
/// Disk-backed modes (`Persistent`, `TempDb`) downgrade to `InMemory` on any
/// I/O, headroom, or corruption failure and carry forward any in-memory entries.
mod storage_machine {
    use std::path::PathBuf;
    use std::sync::Arc;

    use rustc_hash::FxHashMap;

    use crate::db::{CacheDb, normalize_path};
    use crate::tree::TreeArena;
    use crate::types::{ActiveStorageMode, FallbackReason};

    pub type MemCache =
        FxHashMap<(String, PathBuf), (Arc<TreeArena>, u64, u32)>;

    #[derive(Debug)]
    pub enum State {
        Persistent {
            db: Arc<CacheDb>,
            mem_cache: MemCache,
        },
        TempDb {
            db: Arc<CacheDb>,
            mem_cache: MemCache,
        },
        InMemory {
            fallback_reason: Option<FallbackReason>,
            mem_cache: MemCache,
        },
        Streaming,
    }

    #[derive(Debug)]
    pub enum Event {
        StorageFailed(FallbackReason),
    }

    #[derive(Debug, PartialEq, Eq)]
    pub enum Effect {
        EmitFallback {
            from: ActiveStorageMode,
            to: ActiveStorageMode,
            reason: FallbackReason,
        },
    }

    pub fn step(state: State, event: Event) -> (State, Vec<Effect>) {
        match (state, event) {
            (
                State::Persistent { mem_cache, .. },
                Event::StorageFailed(reason),
            ) => (
                State::InMemory {
                    fallback_reason: Some(reason.clone()),
                    mem_cache,
                },
                vec![Effect::EmitFallback {
                    from: ActiveStorageMode::Persistent,
                    to: ActiveStorageMode::InMemory,
                    reason,
                }],
            ),
            (State::TempDb { mem_cache, .. }, Event::StorageFailed(reason)) => {
                (
                    State::InMemory {
                        fallback_reason: Some(reason.clone()),
                        mem_cache,
                    },
                    vec![Effect::EmitFallback {
                        from: ActiveStorageMode::TempDb,
                        to: ActiveStorageMode::InMemory,
                        reason,
                    }],
                )
            }
            (
                State::InMemory { mem_cache, .. },
                Event::StorageFailed(reason),
            ) => (
                State::InMemory {
                    fallback_reason: Some(reason.clone()),
                    mem_cache,
                },
                vec![Effect::EmitFallback {
                    from: ActiveStorageMode::InMemory,
                    to: ActiveStorageMode::InMemory,
                    reason,
                }],
            ),
            (State::Streaming, Event::StorageFailed(_)) => {
                (State::Streaming, vec![])
            }
        }
    }

    impl State {
        pub const fn active_mode(&self) -> ActiveStorageMode {
            match self {
                Self::Persistent { .. } => ActiveStorageMode::Persistent,
                Self::TempDb { .. } => ActiveStorageMode::TempDb,
                Self::InMemory { .. } => ActiveStorageMode::InMemory,
                Self::Streaming => ActiveStorageMode::Streaming,
            }
        }

        pub fn fallback_reason(&self) -> Option<FallbackReason> {
            match self {
                Self::InMemory {
                    fallback_reason, ..
                } => fallback_reason.clone(),
                Self::Persistent { .. }
                | Self::TempDb { .. }
                | Self::Streaming => None,
            }
        }

        pub fn db(&self) -> Option<Arc<CacheDb>> {
            match self {
                Self::Persistent { db, .. } | Self::TempDb { db, .. } => {
                    Some(Arc::clone(db))
                }
                Self::InMemory { .. } | Self::Streaming => None,
            }
        }

        pub const fn mem_cache(&self) -> Option<&MemCache> {
            match self {
                Self::Persistent { mem_cache, .. }
                | Self::TempDb { mem_cache, .. }
                | Self::InMemory { mem_cache, .. } => Some(mem_cache),
                Self::Streaming => None,
            }
        }

        pub const fn mem_cache_mut(&mut self) -> Option<&mut MemCache> {
            match self {
                Self::Persistent { mem_cache, .. }
                | Self::TempDb { mem_cache, .. }
                | Self::InMemory { mem_cache, .. } => Some(mem_cache),
                Self::Streaming => None,
            }
        }

        pub fn invalidate_memory(&mut self, host_key: &str, paths: &[PathBuf]) {
            let Some(cache) = self.mem_cache_mut() else {
                return;
            };
            for path in paths {
                let norm = PathBuf::from(normalize_path(path));
                cache.retain(|(h, cached_path), _| {
                    if h != host_key {
                        return true;
                    }
                    !(cached_path.starts_with(&norm)
                        || norm.starts_with(cached_path))
                });
            }
        }
    }
}

use storage_machine::State as StorageState;

impl StorageState {
    fn open(mode: StorageMode, headroom: u64) -> io::Result<Self> {
        let state = match mode {
            StorageMode::Auto => match default_cache_db_path() {
                Some(path) => match CacheDb::open(path, headroom, false) {
                    Ok(db) => Self::Persistent {
                        db: Arc::new(db),
                        mem_cache: FxHashMap::default(),
                    },
                    Err(reason) => Self::InMemory {
                        fallback_reason: Some(reason),
                        mem_cache: FxHashMap::default(),
                    },
                },
                None => Self::InMemory {
                    fallback_reason: Some(
                        FallbackReason::DatabaseUnavailable {
                            detail: "could not determine user cache directory"
                                .to_owned(),
                        },
                    ),
                    mem_cache: FxHashMap::default(),
                },
            },
            StorageMode::Persistent { path, strict } => {
                match CacheDb::open(path, headroom, false) {
                    Ok(db) => Self::Persistent {
                        db: Arc::new(db),
                        mem_cache: FxHashMap::default(),
                    },
                    Err(reason) if strict => {
                        return Err(io::Error::other(format!(
                            "failed to open cache DB in strict mode: {reason:?}"
                        )));
                    }
                    Err(reason) => Self::InMemory {
                        fallback_reason: Some(reason),
                        mem_cache: FxHashMap::default(),
                    },
                }
            }
            StorageMode::TempDb => match CacheDb::open_temp(headroom) {
                Ok(db) => Self::TempDb {
                    db: Arc::new(db),
                    mem_cache: FxHashMap::default(),
                },
                Err(reason) => Self::InMemory {
                    fallback_reason: Some(reason),
                    mem_cache: FxHashMap::default(),
                },
            },
            StorageMode::InMemory => Self::InMemory {
                fallback_reason: None,
                mem_cache: FxHashMap::default(),
            },
            StorageMode::Streaming => Self::Streaming,
        };
        Ok(state)
    }
}

/// Reusable scanner facade owning the cache DB handle and default policy.
#[derive(Clone, Debug)]
pub struct Scanner {
    storage: Arc<Mutex<StorageState>>,
    default_policy: CachePolicy,
}

impl Scanner {
    /// Open a scanner with the default `StorageMode::Auto` and 15-minute TTL.
    pub fn open_default() -> io::Result<Self> {
        Self::new(StorageMode::Auto, CachePolicy::default())
    }

    /// Open a scanner with explicit [`StorageMode`] and [`CachePolicy`].
    pub fn new(
        mode: StorageMode,
        default_policy: CachePolicy,
    ) -> io::Result<Self> {
        let runtime =
            StorageState::open(mode, default_policy.min_free_headroom)?;
        Ok(Self {
            storage: Arc::new(Mutex::new(runtime)),
            default_policy,
        })
    }

    pub fn active_mode(&self) -> ActiveStorageMode {
        self.storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active_mode()
    }

    pub fn fallback_reason(&self) -> Option<FallbackReason> {
        self.storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fallback_reason()
    }

    pub const fn default_policy(&self) -> &CachePolicy {
        &self.default_policy
    }

    /// Start an SWR [`ScanSession`] for local `root` using the scanner's
    /// default [`CachePolicy`].
    pub fn open(
        &self,
        root: PathBuf,
        options: ScanOptions,
    ) -> io::Result<ScanSession> {
        self.open_with(HostKey::Local, root, options, self.default_policy)
    }

    /// Start an SWR [`ScanSession`] with an explicit host namespace and policy.
    pub fn open_with(
        &self,
        host: HostKey,
        root: PathBuf,
        options: ScanOptions,
        policy: CachePolicy,
    ) -> io::Result<ScanSession> {
        self.open_with_tally(host, root, options, policy, None)
    }

    /// Start an SWR [`ScanSession`] with an optional per-directory tally hook
    /// (for live treemap previews during cold walks).
    pub fn open_with_tally(
        &self,
        host: HostKey,
        root: PathBuf,
        options: ScanOptions,
        policy: CachePolicy,
        on_dir_tally: Option<DirTallyHook>,
    ) -> io::Result<ScanSession> {
        let started = Instant::now();
        let canonical_root = root.canonicalize().unwrap_or(root);
        let (event_tx, event_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let pause_gate = Arc::new(PauseGate::new());
        let progress = Arc::new(WalkProgress::default());
        let completed_immediate: Arc<Mutex<FxHashMap<PathBuf, TreeArena>>> =
            Arc::new(Mutex::new(FxHashMap::default()));

        let (db_opt, active_mode, fallback_reason) = {
            let guard = self
                .storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (guard.db(), guard.active_mode(), guard.fallback_reason())
        };

        // 1. Instant cache check (sub-10ms fast path).
        let now = unix_now();
        let mut needs_walk = true;
        if !policy.force_refresh && active_mode != ActiveStorageMode::Streaming
        {
            if let Some(cached) = self.try_load_cached(
                db_opt.as_ref(),
                &host,
                &canonical_root,
                &options,
                now,
                &policy,
                &event_tx,
            ) {
                let root_node = cached.arena.node(cached.arena.root());
                let stats = ScanStats {
                    files: root_node.files,
                    dirs: u64::from(root_node.dirs),
                    allocated_bytes: root_node.allocated_bytes,
                    apparent_bytes: root_node.apparent_bytes,
                    errors: 0,
                    skipped_fresh_dirs: u64::from(root_node.dirs),
                    walked_dirs: 0,
                    duration_ms: started.elapsed().as_millis() as u64,
                };
                let is_fresh =
                    matches!(cached.freshness, Freshness::Fresh { .. });

                if is_fresh || policy.stale_while_revalidate {
                    let (cur_mode, cur_reason) = {
                        let guard = self
                            .storage
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        (guard.active_mode(), guard.fallback_reason())
                    };
                    let _ = event_tx.send(ScanEvent::Snapshot(ScanSnapshot {
                        arena: Arc::new(cached.arena),
                        freshness: cached.freshness,
                        stats,
                        storage_mode: cur_mode,
                        fallback_reason: cur_reason,
                    }));
                }

                if is_fresh {
                    needs_walk = false;
                    let _ = event_tx.send(ScanEvent::Finished);
                }
            }
        } else if let Some(reason) = fallback_reason {
            // Surface initial fallback if caller opened in Auto and it already
            // downgraded before the session began.
            let _ = event_tx.send(ScanEvent::StorageFallback {
                from: ActiveStorageMode::Persistent,
                to: active_mode,
                reason,
            });
        }

        // 2. Spawn the immediate-priority command dispatcher (always available
        //    so `prioritize`/`revalidate` work both mid-scan and post-hit).
        //    Holds a weak storage ref so dropping the Scanner releases TempDb
        //    deterministically without racing the idle loop.
        {
            let storage = Arc::downgrade(&self.storage);
            let host = host.clone();
            let cancel = Arc::clone(&cancel);
            let pause_gate = Arc::clone(&pause_gate);
            let completed_immediate = Arc::clone(&completed_immediate);
            let event_tx = event_tx.clone();
            thread::Builder::new()
                .name("disktree-priority".into())
                .spawn(move || {
                    run_priority_loop(
                        &cmd_rx,
                        &storage,
                        &host,
                        &options,
                        &cancel,
                        &pause_gate,
                        &completed_immediate,
                        &event_tx,
                    );
                })?;
        }

        // 3. Spawn background revalidation walk if needed.
        if needs_walk {
            let storage = Arc::clone(&self.storage);
            let host = host.clone();
            let walk_root = canonical_root.clone();
            let cancel = Arc::clone(&cancel);
            let pause_gate = Arc::clone(&pause_gate);
            let progress = Arc::clone(&progress);
            let completed_immediate = Arc::clone(&completed_immediate);

            thread::Builder::new()
                .name("disktree-bg-scan".into())
                .spawn(move || {
                    run_background_walk(
                        &storage,
                        &host,
                        &walk_root,
                        &options,
                        &policy,
                        &cancel,
                        &pause_gate,
                        &progress,
                        &completed_immediate,
                        on_dir_tally,
                        &event_tx,
                    );
                })?;
        }

        Ok(ScanSession {
            root: canonical_root,
            host,
            events: event_rx,
            commands: cmd_tx,
            cancel,
            pause_gate,
            progress,
            storage: Arc::clone(&self.storage),
        })
    }

    /// Blocking convenience query: returns the fresh snapshot (or immediate
    /// stale snapshot if `accept_stale` is true and cached data exists).
    pub fn scan_blocking(
        &self,
        root: &Path,
        options: ScanOptions,
        policy: CachePolicy,
        accept_stale: bool,
    ) -> io::Result<ScanSnapshot> {
        let session = self.open_with(
            HostKey::Local,
            root.to_path_buf(),
            options,
            policy,
        )?;
        let mut latest: Option<ScanSnapshot> = None;
        while let Ok(event) = session.recv() {
            match event {
                ScanEvent::Snapshot(snap) => {
                    let complete_cache = matches!(
                        snap.freshness,
                        Freshness::Fresh { .. } | Freshness::Stale { .. }
                    );
                    if matches!(snap.freshness, Freshness::Fresh { .. })
                        || (accept_stale && complete_cache)
                    {
                        session.cancel();
                        return Ok(snap);
                    }
                    latest = Some(snap);
                }
                ScanEvent::Error { message } => {
                    if latest.is_none() {
                        return Err(io::Error::other(message));
                    }
                }
                ScanEvent::Finished => break,
                _ => {}
            }
        }
        latest.ok_or_else(|| {
            io::Error::other("scan completed without producing a snapshot")
        })
    }

    /// Run a bounded-memory `O(depth)` streaming walk without retaining a tree
    /// or touching the cache DB.
    pub fn stream<F>(
        &self,
        root: &Path,
        options: &ScanOptions,
        cancel: Option<&Arc<AtomicBool>>,
        emit: F,
    ) -> io::Result<()>
    where
        F: FnMut(StreamItem) -> io::Result<()>,
    {
        stream_walk(root, options, cancel, emit)
    }

    /// Mark one or more paths dirty in the cache (e.g. after files were
    /// deleted) so subsequent scans revalidate them immediately.
    pub fn invalidate(
        &self,
        host: &HostKey,
        paths: &[PathBuf],
    ) -> io::Result<InvalidateReport> {
        let host_key = host.as_db_key();
        let db_opt = {
            let mut guard = self
                .storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.invalidate_memory(&host_key, paths);
            guard.db()
        };

        let Some(db) = db_opt else {
            return Ok(InvalidateReport::default());
        };
        match db.invalidate(host, paths) {
            Ok(report) => Ok(report),
            Err(reason) => {
                downgrade_storage(&self.storage, reason);
                Ok(InvalidateReport::default())
            }
        }
    }

    /// Look up a cached subtree for `host` and `root` without starting a walk.
    ///
    /// Useful for remote SSH hosts whose live walk runs over the network rather
    /// than on the local filesystem.
    pub fn lookup_cached(
        &self,
        host: &HostKey,
        root: &Path,
        options: &ScanOptions,
        policy: &CachePolicy,
    ) -> Option<(Arc<TreeArena>, Freshness)> {
        let db_opt = self
            .storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .db();
        let (tx, _rx) = mpsc::channel();
        let cached = self.try_load_cached(
            db_opt.as_ref(),
            host,
            root,
            options,
            unix_now(),
            policy,
            &tx,
        )?;
        Some((Arc::new(cached.arena), cached.freshness))
    }

    /// Persist a completed snapshot (for example, a finished remote SSH scan)
    /// into the in-memory cache immediately and into the on-disk cache DB in
    /// the background.
    pub fn store_snapshot(
        &self,
        host: &HostKey,
        arena: &Arc<TreeArena>,
        options: &ScanOptions,
        scanned_at: u64,
    ) {
        let db_opt = {
            let mut guard = self
                .storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cache) = guard.mem_cache_mut() {
                cache.insert(
                    (host.as_db_key(), arena.root_path().to_path_buf()),
                    (
                        Arc::clone(arena),
                        scanned_at,
                        options.cache_topology_mask(),
                    ),
                );
            }
            guard.db()
        };
        if let Some(db) = db_opt {
            let storage = Arc::clone(&self.storage);
            let host = host.clone();
            let arena = Arc::clone(arena);
            let options = *options;
            let _ = thread::Builder::new()
                .name("disktree-cache-write".into())
                .spawn(move || {
                    if let Err(reason) =
                        db.store_subtree(&host, &arena, &options, scanned_at)
                    {
                        downgrade_storage(&storage, reason);
                    }
                });
        }
    }

    /// Return diagnostic stats for the persistent/temp cache database.
    pub fn cache_stats(&self) -> io::Result<CacheStats> {
        let db_opt = self
            .storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .db();
        let Some(db) = db_opt else {
            return Ok(CacheStats::default());
        };
        db.stats()
            .map_err(|reason| io::Error::other(format!("{reason:?}")))
    }

    /// Clear all cached data from the DB and in-memory cache.
    pub fn clear_cache(&self) -> io::Result<()> {
        let db_opt = {
            let mut guard = self
                .storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cache) = guard.mem_cache_mut() {
                cache.clear();
            }
            guard.db()
        };
        if let Some(db) = db_opt {
            db.clear()
                .map_err(|reason| io::Error::other(format!("{reason:?}")))?;
        }
        Ok(())
    }

    /// Prune cached chunks older than `max_age`.
    pub fn prune_cache(&self, max_age: Duration) -> io::Result<u64> {
        let db_opt = self
            .storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .db();
        let Some(db) = db_opt else {
            return Ok(0);
        };
        db.prune_older_than(unix_now(), max_age.as_secs())
            .map_err(|reason| io::Error::other(format!("{reason:?}")))
    }

    #[allow(clippy::too_many_arguments, reason = "internal cache read helper")]
    fn try_load_cached(
        &self,
        db_opt: Option<&Arc<CacheDb>>,
        host: &HostKey,
        canonical_root: &Path,
        options: &ScanOptions,
        now: u64,
        policy: &CachePolicy,
        event_tx: &Sender<ScanEvent>,
    ) -> Option<crate::db::CachedSubtree> {
        if let Some(db) = db_opt {
            match db.load_subtree(host, canonical_root, options, now, policy) {
                Ok(found) => return found,
                Err(reason) => {
                    let from = downgrade_storage(&self.storage, reason.clone());
                    let _ = event_tx.send(ScanEvent::StorageFallback {
                        from,
                        to: ActiveStorageMode::InMemory,
                        reason,
                    });
                }
            }
        }

        // Fallback in-memory cache lookup.
        let host_key = host.as_db_key();
        let entry = {
            let guard = self
                .storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard
                .mem_cache()
                .and_then(|cache| {
                    cache.get(&(host_key, canonical_root.to_path_buf()))
                })
                .cloned()
        };
        let (arena, scanned_at, mask) = entry?;
        if mask != options.cache_topology_mask() {
            return None;
        }
        let mut cloned = (*arena).clone();
        cloned.aggregate(options.dedup_hardlinks, options.include_hidden);
        cloned.sort_children(options.byte_metric());
        let ttl_secs = policy.ttl.as_secs();
        cloned.refresh_statuses(now, ttl_secs);
        let freshness = if now.saturating_sub(scanned_at) <= ttl_secs {
            Freshness::Fresh {
                oldest_at: scanned_at,
                newest_at: scanned_at,
            }
        } else {
            Freshness::Stale {
                oldest_at: scanned_at,
                newest_at: scanned_at,
                stale_dirs: cloned.node(cloned.root()).dirs,
            }
        };
        Some(crate::db::CachedSubtree {
            arena: cloned,
            freshness,
        })
    }
}

enum SessionCmd {
    Revalidate {
        paths: Vec<PathBuf>,
        priority: Priority,
    },
}

/// Active handle to a running (or cache-served) scan session.
pub struct ScanSession {
    root: PathBuf,
    host: HostKey,
    events: Receiver<ScanEvent>,
    commands: Sender<SessionCmd>,
    cancel: Arc<AtomicBool>,
    pause_gate: Arc<PauseGate>,
    progress: Arc<WalkProgress>,
    storage: Arc<Mutex<StorageState>>,
}

impl std::fmt::Debug for ScanSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanSession")
            .field("root", &self.root)
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl Drop for ScanSession {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl ScanSession {
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Poll for the next [`ScanEvent`] without blocking (ideal for GUI/TUI
    /// render loops).
    pub fn try_recv(&self) -> Result<ScanEvent, TryRecvError> {
        self.events.try_recv()
    }

    /// Wait for the next [`ScanEvent`].
    pub fn recv(&self) -> Result<ScanEvent, mpsc::RecvError> {
        self.events.recv()
    }

    /// Wait with a timeout for the next [`ScanEvent`].
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<ScanEvent, mpsc::RecvTimeoutError> {
        self.events.recv_timeout(timeout)
    }

    /// Live lock-free progress snapshot of any in-flight background walk.
    pub fn progress_snapshot(&self, duration_ms: u64) -> ScanStats {
        self.progress.snapshot(duration_ms)
    }

    /// Shared atomic progress handle for live counters and error details.
    pub fn progress(&self) -> Arc<WalkProgress> {
        Arc::clone(&self.progress)
    }

    /// Preempt background revalidation immediately to refresh `path` first,
    /// then automatically resume background work right where it left off.
    pub fn prioritize(&self, path: &Path) -> io::Result<()> {
        self.revalidate(&[path.to_path_buf()], Priority::Immediate)
    }

    /// Request targeted revalidation of specific subtrees regardless of TTL.
    /// `Priority::Immediate` pauses background admission until these paths
    /// complete, commits them fresh, and resumes background work afterwards.
    pub fn revalidate(
        &self,
        paths: &[PathBuf],
        priority: Priority,
    ) -> io::Result<()> {
        self.commands
            .send(SessionCmd::Revalidate {
                paths: paths.to_vec(),
                priority,
            })
            .map_err(|_| {
                io::Error::other("priority dispatcher is no longer running")
            })
    }

    /// Mark paths dirty in the cache immediately (e.g. right after deletion).
    pub fn invalidate(
        &self,
        paths: &[PathBuf],
    ) -> io::Result<InvalidateReport> {
        let host_key = self.host.as_db_key();
        let db_opt = {
            let mut guard = self
                .storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.invalidate_memory(&host_key, paths);
            guard.db()
        };
        let Some(db) = db_opt else {
            return Ok(InvalidateReport::default());
        };
        match db.invalidate(&self.host, paths) {
            Ok(report) => Ok(report),
            Err(reason) => {
                downgrade_storage(&self.storage, reason);
                Ok(InvalidateReport::default())
            }
        }
    }

    /// Cooperatively cancel in-flight background and immediate work.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.pause_gate.cancel();
    }
}

#[allow(clippy::too_many_arguments, reason = "worker thread parameters")]
fn run_background_walk(
    storage: &Arc<Mutex<StorageState>>,
    host: &HostKey,
    walk_root: &Path,
    options: &ScanOptions,
    policy: &CachePolicy,
    cancel: &Arc<AtomicBool>,
    pause_gate: &Arc<PauseGate>,
    progress: &Arc<WalkProgress>,
    completed_immediate: &Arc<Mutex<FxHashMap<PathBuf, TreeArena>>>,
    on_dir_tally: Option<DirTallyHook>,
    event_tx: &Sender<ScanEvent>,
) {
    let now = unix_now();
    let ttl_secs = policy.ttl.as_secs();

    let db_opt = storage
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .db();

    // Preload fresh (skip) and stale (defer-last, oldest-first) subtrees
    // strictly beneath `walk_root`. Even on `force_refresh`, loading stale
    // timestamps lets brand-new unscanned directories walk ahead of cached ones.
    let catalog = if let Some(db) = &db_opt {
        let effective_ttl =
            if policy.reuse_fresh_subtrees && !policy.force_refresh {
                ttl_secs
            } else {
                0
            };
        match db.cached_subtrees_below(
            host,
            walk_root,
            options,
            now,
            effective_ttl,
            &FxHashSet::default(),
        ) {
            Ok(mut catalog) => {
                if !policy.reuse_fresh_subtrees || policy.force_refresh {
                    // Nothing is skipped on force_refresh; treat any
                    // just-written `now` entries as stale too so unscanned
                    // paths still run first.
                    for (path, _) in catalog.fresh.drain() {
                        catalog.stale.insert(path, now);
                    }
                }
                catalog
            }
            Err(reason) => {
                let from = downgrade_storage(storage, reason.clone());
                let _ = event_tx.send(ScanEvent::StorageFallback {
                    from,
                    to: ActiveStorageMode::InMemory,
                    reason,
                });
                crate::db::SubtreeCatalog::default()
            }
        }
    } else {
        crate::db::SubtreeCatalog::default()
    };

    let loader: Option<ShardLoader> = db_opt.as_ref().map(|db| {
        let db = Arc::clone(db);
        let host = host.clone();
        let f: ShardLoader = Arc::new(move |dir_id: u64, path: &Path| {
            db.load_shard_by_dir_id(
                &host,
                dir_id,
                path.to_path_buf(),
                now,
                ttl_secs,
            )
            .ok()
            .flatten()
        });
        f
    });

    let oracle: Arc<dyn SkipOracle> = Arc::new(MapSkipOracle {
        in_memory: Arc::clone(completed_immediate),
        db_fresh: catalog.fresh,
        db_stale: catalog.stale,
        loader,
    });

    // Notify progress ticker / directory updates lazily via hook.
    let last_emit = Arc::new(Mutex::new(Instant::now()));
    let hook_tx = event_tx.clone();
    let hook_progress = Arc::clone(progress);
    let hook_started = Instant::now();
    let on_dir_start: DirStartHook = Arc::new(move |dir: &Path| {
        if let Ok(mut last) = last_emit.try_lock()
            && last.elapsed() >= Duration::from_millis(80)
        {
            *last = Instant::now();
            let _ = hook_tx.send(ScanEvent::Progress {
                stats: hook_progress
                    .snapshot(hook_started.elapsed().as_millis() as u64),
                active_path: Some(dir.to_path_buf()),
            });
        }
    });

    let outcome = match walk_parallel(
        walk_root,
        WalkConfig {
            options,
            cancel: Arc::clone(cancel),
            progress: Arc::clone(progress),
            pause_gate: Some(Arc::clone(pause_gate)),
            skip_oracle: Some(oracle),
            on_dir_start: Some(on_dir_start),
            on_dir_tally,
        },
    ) {
        Ok(outcome) => outcome,
        Err(error) => {
            let _ = event_tx.send(ScanEvent::Error {
                message: error.to_string(),
            });
            let _ = event_tx.send(ScanEvent::Finished);
            return;
        }
    };

    if outcome.cancelled {
        let _ = event_tx.send(ScanEvent::Finished);
        return;
    }

    let arena = Arc::new(outcome.arena);
    let (active_mode, fallback_reason) = {
        let guard = storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (guard.active_mode(), guard.fallback_reason())
    };

    // Deliver the live tree to interactive UIs immediately before committing
    // Protobuf shards to the cache DB on disk.
    let _ = event_tx.send(ScanEvent::Snapshot(ScanSnapshot {
        arena: Arc::clone(&arena),
        freshness: Freshness::Live {
            scanned_at: outcome.scanned_at,
        },
        stats: outcome.stats,
        storage_mode: active_mode,
        fallback_reason,
    }));

    persist_arena(storage, host, &arena, options, outcome.scanned_at, event_tx);
    let _ = event_tx.send(ScanEvent::Finished);
}

#[allow(clippy::too_many_arguments, reason = "priority worker loop state")]
fn run_priority_loop(
    cmd_rx: &Receiver<SessionCmd>,
    storage: &Weak<Mutex<StorageState>>,
    host: &HostKey,
    options: &ScanOptions,
    cancel: &Arc<AtomicBool>,
    pause_gate: &Arc<PauseGate>,
    completed_immediate: &Arc<Mutex<FxHashMap<PathBuf, TreeArena>>>,
    event_tx: &Sender<ScanEvent>,
) {
    while let Ok(cmd) = cmd_rx.recv() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let Some(storage) = storage.upgrade() else {
            break;
        };
        let storage = &storage;
        match cmd {
            SessionCmd::Revalidate { paths, priority } => {
                for raw_path in paths {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let target = raw_path.canonicalize().unwrap_or(raw_path);
                    let preempted = priority == Priority::Immediate;
                    if preempted {
                        pause_gate.pause();
                    }

                    let _ = event_tx.send(ScanEvent::SubtreeUpdating {
                        path: target.clone(),
                        priority,
                    });

                    let sub_progress = Arc::new(WalkProgress::default());
                    let walk_res = walk_parallel(
                        &target,
                        WalkConfig {
                            options,
                            cancel: Arc::clone(cancel),
                            progress: sub_progress,
                            pause_gate: None,
                            skip_oracle: None,
                            on_dir_start: None,
                            on_dir_tally: None,
                        },
                    );

                    match walk_res {
                        Ok(outcome) if !outcome.cancelled => {
                            let arena = Arc::new(outcome.arena);
                            persist_arena(
                                storage,
                                host,
                                &arena,
                                options,
                                outcome.scanned_at,
                                event_tx,
                            );
                            completed_immediate
                                .lock()
                                .unwrap_or_else(
                                    std::sync::PoisonError::into_inner,
                                )
                                .insert(target.clone(), (*arena).clone());

                            let _ =
                                event_tx.send(ScanEvent::SubtreeRefreshed {
                                    path: target,
                                    arena,
                                    scanned_at: outcome.scanned_at,
                                    priority,
                                });
                        }
                        Ok(_) => {}
                        Err(err) => {
                            let _ = event_tx.send(ScanEvent::Error {
                                message: format!("{}: {err}", target.display()),
                            });
                        }
                    }

                    if preempted {
                        pause_gate.resume();
                    }
                }
            }
        }
    }
}

fn persist_arena(
    storage: &Arc<Mutex<StorageState>>,
    host: &HostKey,
    arena: &Arc<TreeArena>,
    options: &ScanOptions,
    scanned_at: u64,
    event_tx: &Sender<ScanEvent>,
) {
    let db_opt = {
        let mut guard = storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cache) = guard.mem_cache_mut() {
            cache.insert(
                (host.as_db_key(), arena.root_path().to_path_buf()),
                (Arc::clone(arena), scanned_at, options.cache_topology_mask()),
            );
        }
        guard.db()
    };

    if let Some(db) = db_opt
        && let Err(reason) = db.store_subtree(host, arena, options, scanned_at)
    {
        let from = downgrade_storage(storage, reason.clone());
        let _ = event_tx.send(ScanEvent::StorageFallback {
            from,
            to: ActiveStorageMode::InMemory,
            reason,
        });
    }
}

fn downgrade_storage(
    storage: &Arc<Mutex<StorageState>>,
    reason: FallbackReason,
) -> ActiveStorageMode {
    let mut guard = storage
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let current = std::mem::replace(&mut *guard, StorageState::Streaming);
    let (next, effects) = storage_machine::step(
        current,
        storage_machine::Event::StorageFailed(reason),
    );
    *guard = next;
    drop(guard);
    effects
        .into_iter()
        .next()
        .map_or(ActiveStorageMode::Streaming, |effect| match effect {
            storage_machine::Effect::EmitFallback { from, .. } => from,
        })
}

#[cfg(test)]
mod machine_tests {
    use super::storage_machine::{Effect, Event, State, step};
    use super::*;

    #[test]
    fn storage_machine_downgrades_and_keeps_streaming_pure() {
        let (state, effects) = step(
            State::InMemory {
                fallback_reason: None,
                mem_cache: FxHashMap::default(),
            },
            Event::StorageFailed(FallbackReason::DiskFull),
        );
        assert_eq!(state.active_mode(), ActiveStorageMode::InMemory);
        assert_eq!(state.fallback_reason(), Some(FallbackReason::DiskFull));
        assert_eq!(
            effects,
            vec![Effect::EmitFallback {
                from: ActiveStorageMode::InMemory,
                to: ActiveStorageMode::InMemory,
                reason: FallbackReason::DiskFull,
            }]
        );

        let (state, effects) = step(
            State::Streaming,
            Event::StorageFailed(FallbackReason::DiskFull),
        );
        assert_eq!(state.active_mode(), ActiveStorageMode::Streaming);
        assert!(effects.is_empty());
    }
}

#[cfg(all(test, feature = "cache-db"))]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn repeated_scan_hits_fresh_cache_and_partial_widen_skips_fresh_subtree() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("cache.redb");
        let tree_root = temp.path().join("fs");
        let sub_a = tree_root.join("a");
        let sub_b = tree_root.join("b");
        fs::create_dir_all(&sub_a).expect("mkdir a");
        fs::create_dir_all(&sub_b).expect("mkdir b");
        fs::write(sub_a.join("file_a.bin"), vec![1u8; 4096]).expect("write a");
        fs::write(sub_b.join("file_b.bin"), vec![2u8; 8192]).expect("write b");

        let scanner = Scanner::new(
            StorageMode::Persistent {
                path: db_path,
                strict: true,
            },
            CachePolicy {
                min_free_headroom: 0,
                ..CachePolicy::default()
            },
        )
        .expect("scanner");

        // 1. Scan only `a` first.
        let snap_a = scanner
            .scan_blocking(
                &sub_a,
                ScanOptions::default(),
                CachePolicy {
                    min_free_headroom: 0,
                    ..CachePolicy::default()
                },
                false,
            )
            .expect("scan a");
        assert!(matches!(snap_a.freshness, Freshness::Live { .. }));
        assert_eq!(snap_a.stats.apparent_bytes, 4096);

        // 2. Re-scanning `a` within 15 minutes hits cache with 0 walked dirs.
        let hit_a = scanner
            .scan_blocking(
                &sub_a,
                ScanOptions::default(),
                CachePolicy {
                    min_free_headroom: 0,
                    ..CachePolicy::default()
                },
                false,
            )
            .expect("hit a");
        assert!(matches!(hit_a.freshness, Freshness::Fresh { .. }));
        assert_eq!(hit_a.stats.walked_dirs, 0);
        assert_eq!(hit_a.stats.apparent_bytes, 4096);

        // 3. Widening to `tree_root` skips `a` (reused from cache) and only
        //    walks `tree_root` + `b`!
        let wide = scanner
            .scan_blocking(
                &tree_root,
                ScanOptions::default(),
                CachePolicy {
                    min_free_headroom: 0,
                    ..CachePolicy::default()
                },
                false,
            )
            .expect("wide");
        assert!(matches!(wide.freshness, Freshness::Live { .. }));
        assert_eq!(wide.stats.apparent_bytes, 4096 + 8192);
        assert!(
            wide.stats.skipped_fresh_dirs >= 1,
            "expected `a` to be skipped from cache, stats: {:?}",
            wide.stats
        );
    }

    #[test]
    fn stale_while_revalidate_emits_stale_first_then_fresh() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("swr.redb");
        let root = temp.path().join("data");
        fs::create_dir_all(&root).expect("mkdir");
        fs::write(root.join("v1.txt"), b"hello").expect("write");

        let scanner = Scanner::new(
            StorageMode::Persistent {
                path: db_path,
                strict: true,
            },
            CachePolicy {
                min_free_headroom: 0,
                ..CachePolicy::default()
            },
        )
        .expect("scanner");

        // Warm cache.
        let _ = scanner
            .scan_blocking(
                &root,
                ScanOptions::default(),
                CachePolicy {
                    min_free_headroom: 0,
                    ..CachePolicy::default()
                },
                false,
            )
            .expect("warm");

        // Mutate disk after caching.
        fs::write(root.join("v2.txt"), b"world!").expect("write v2");

        // Sleep 1s and query with TTL = 0s so cached entry is strictly stale.
        thread::sleep(Duration::from_secs(1));
        let session = scanner
            .open_with(
                HostKey::Local,
                root,
                ScanOptions::default(),
                CachePolicy {
                    ttl: Duration::ZERO,
                    stale_while_revalidate: true,
                    reuse_fresh_subtrees: false,
                    min_free_headroom: 0,
                    ..CachePolicy::default()
                },
            )
            .expect("open swr");

        let mut snapshots = Vec::new();
        while let Ok(ev) = session.recv() {
            match ev {
                ScanEvent::Snapshot(snap) => snapshots.push(snap),
                ScanEvent::Finished => break,
                _ => {}
            }
        }
        assert_eq!(snapshots.len(), 2, "expected stale then live snapshot");
        assert!(matches!(snapshots[0].freshness, Freshness::Stale { .. }));
        assert_eq!(snapshots[0].stats.apparent_bytes, 5);
        assert!(matches!(snapshots[1].freshness, Freshness::Live { .. }));
        assert_eq!(snapshots[1].stats.apparent_bytes, 11);
    }

    #[test]
    fn targeted_prioritize_refreshes_subtree_and_emits_events() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("prio.redb");
        let root = temp.path().join("root");
        let focus = root.join("focus");
        fs::create_dir_all(&focus).expect("mkdir");
        fs::write(focus.join("a.bin"), vec![0u8; 100]).expect("write");

        let scanner = Scanner::new(
            StorageMode::Persistent {
                path: db_path,
                strict: true,
            },
            CachePolicy {
                min_free_headroom: 0,
                ..CachePolicy::default()
            },
        )
        .expect("scanner");

        let _ = scanner
            .scan_blocking(
                &root,
                ScanOptions::default(),
                CachePolicy {
                    min_free_headroom: 0,
                    ..CachePolicy::default()
                },
                false,
            )
            .expect("warm");

        // Add new file inside `focus` and request immediate prioritization.
        fs::write(focus.join("b.bin"), vec![0u8; 250]).expect("write b");

        let session = scanner.open(root, ScanOptions::default()).expect("open");
        // Drain initial fresh snapshot + finished.
        let mut saw_initial = false;
        while let Ok(ev) = session.recv_timeout(Duration::from_secs(2)) {
            if matches!(ev, ScanEvent::Snapshot(_)) {
                saw_initial = true;
            }
            if matches!(ev, ScanEvent::Finished) {
                break;
            }
        }
        assert!(saw_initial);

        session.prioritize(&focus).expect("prioritize");

        let mut saw_updating = false;
        let mut refreshed_bytes = None;
        while let Ok(ev) = session.recv_timeout(Duration::from_secs(5)) {
            match ev {
                ScanEvent::SubtreeUpdating { priority, .. } => {
                    assert_eq!(priority, Priority::Immediate);
                    saw_updating = true;
                }
                ScanEvent::SubtreeRefreshed {
                    arena, priority, ..
                } => {
                    assert_eq!(priority, Priority::Immediate);
                    refreshed_bytes =
                        Some(arena.node(arena.root()).apparent_bytes);
                    break;
                }
                _ => {}
            }
        }
        assert!(saw_updating);
        assert_eq!(refreshed_bytes, Some(350));
    }

    #[test]
    fn automatic_fallback_when_disk_headroom_unmet_or_unwritable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("data");
        fs::create_dir_all(&root).expect("mkdir");
        fs::write(root.join("x.txt"), b"ok").expect("write");

        // 1. Trigger headroom fallback with impossible requirement.
        let scanner = Scanner::new(
            StorageMode::Persistent {
                path: temp.path().join("too-tight.redb"),
                strict: false,
            },
            CachePolicy {
                min_free_headroom: u64::MAX,
                ..CachePolicy::default()
            },
        )
        .expect("scanner falls back cleanly");
        assert_eq!(scanner.active_mode(), ActiveStorageMode::InMemory);
        assert!(matches!(
            scanner.fallback_reason(),
            Some(FallbackReason::LowDiskSpace { .. })
        ));

        let snap = scanner
            .scan_blocking(
                &root,
                ScanOptions::default(),
                CachePolicy::default(),
                false,
            )
            .expect("scan succeeds in memory");
        assert_eq!(snap.storage_mode, ActiveStorageMode::InMemory);
        assert_eq!(snap.stats.apparent_bytes, 2);

        // 2. Trigger unwritable path fallback (DB path inside a regular file).
        let blocker = temp.path().join("not-a-dir");
        fs::write(&blocker, b"file").expect("write blocker");
        let scanner2 = Scanner::new(
            StorageMode::Persistent {
                path: blocker.join("cache.redb"),
                strict: false,
            },
            CachePolicy {
                min_free_headroom: 0,
                ..CachePolicy::default()
            },
        )
        .expect("scanner2 falls back cleanly");
        assert_eq!(scanner2.active_mode(), ActiveStorageMode::InMemory);
        assert!(scanner2.fallback_reason().is_some());
    }

    #[test]
    fn invalidate_forces_fresh_rescan_of_deleted_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("inv.redb");
        let root = temp.path().join("root");
        let gone = root.join("gone");
        fs::create_dir_all(&gone).expect("mkdir");
        fs::write(gone.join("big.bin"), vec![9u8; 4096]).expect("write");

        let policy = CachePolicy {
            min_free_headroom: 0,
            ..CachePolicy::default()
        };
        let scanner = Scanner::new(
            StorageMode::Persistent {
                path: db_path,
                strict: true,
            },
            policy,
        )
        .expect("scanner");

        let first = scanner
            .scan_blocking(&root, ScanOptions::default(), policy, false)
            .expect("first");
        assert_eq!(first.stats.apparent_bytes, 4096);

        // Delete `gone` externally and call `invalidate`.
        fs::remove_dir_all(&gone).expect("remove");
        scanner
            .invalidate(&HostKey::Local, &[gone])
            .expect("invalidate");

        // Next scan sees 0 bytes immediately (does not serve stale 4096).
        let second = scanner
            .scan_blocking(&root, ScanOptions::default(), policy, false)
            .expect("second");
        assert_eq!(second.stats.apparent_bytes, 0);
    }
}

//! Parallel Rayon filesystem walker with cache-skip grafting and
//! priority-interruptible pausing.
//!
//! Follows `disktree`'s core invariants:
//! - One `rayon::scope` per scan root so stack depth is O(1).
//! - `+1` completion sentinel on every directory so no directory is assembled
//!   before both its direct scan and every child task finish (Invariant #3).
//! - Both `allocated_bytes` and `apparent_bytes` captured in a single metadata
//!   pass (Invariant #1), and hidden entries flagged rather than dropped from
//!   raw storage so `-a` and `-H` toggle instantly without rescanning.
//! - Lock-free `CacheSkipOracle` checks fresh subtrees before touching disk,
//!   and `PauseGate` parks background workers cleanly whenever a user-focused
//!   `Priority::Immediate` subtree preempts the refresh.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use rayon::Scope;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::mounts::{mount_boundaries_for, never_scanned};
#[cfg(not(target_os = "linux"))]
use crate::stream::measure_both;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
use crate::stream::nlink_of;
use crate::stream::{device_of, inode_of, modified_seconds};
use crate::tree::{NodeFlags, NodeId, TreeArena};
use crate::types::{NodeKind, ScanOptions, ScanStats, SubtreeStatus, unix_now};

/// Callback invoked right as a directory begins walking.
pub type DirStartHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// Callback invoked once per walked directory with its direct file totals:
/// `(dir_path, allocated_bytes, apparent_bytes, files)`.
pub type DirTallyHook = Arc<dyn Fn(&Path, u64, u64, u64) + Send + Sync>;

/// Callback invoked when a complete, uncancelled subtree finishes walking
/// (used for cadence-based incremental DB checkpointing and live UI updates):
/// `(dir_path, depth, materialized_arena)`.
pub type DirCompleteHook =
    Arc<dyn Fn(PathBuf, usize, Arc<TreeArena>) + Send + Sync>;

/// Callback loading a cached shard by `dir_id` and path.
pub type ShardLoader =
    Arc<dyn Fn(u64, &Path) -> Option<TreeArena> + Send + Sync>;

/// Maximum number of per-path error strings retained on [`WalkProgress`].
const MAX_ERROR_DETAIL: usize = 50;

/// Pure state machine for [`PauseGate`].
///
/// Atomic transition: `step(state, event) -> (state, Vec<Effect>)`.
/// `Cancelled` is terminal: later `Pause` or `Resume` events are ignored.
mod gate_machine {
    use std::num::NonZeroUsize;

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum State {
        #[default]
        Running,
        Paused {
            holds: NonZeroUsize,
        },
        Cancelled,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Event {
        Pause,
        Resume,
        Cancel,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Effect {
        WakeAll,
    }

    pub fn step(state: State, event: Event) -> (State, Vec<Effect>) {
        match (state, event) {
            (State::Running, Event::Pause) => (
                State::Paused {
                    holds: NonZeroUsize::MIN,
                },
                vec![],
            ),
            (State::Paused { holds }, Event::Pause) => (
                State::Paused {
                    holds: holds.saturating_add(1),
                },
                vec![],
            ),
            (State::Paused { holds }, Event::Resume) => {
                match NonZeroUsize::new(holds.get() - 1) {
                    Some(remaining) => {
                        (State::Paused { holds: remaining }, vec![])
                    }
                    None => (State::Running, vec![Effect::WakeAll]),
                }
            }
            (State::Running, Event::Resume) => (State::Running, vec![]),
            (State::Running | State::Paused { .. }, Event::Cancel) => {
                (State::Cancelled, vec![Effect::WakeAll])
            }
            (
                State::Cancelled,
                Event::Pause | Event::Resume | Event::Cancel,
            ) => (State::Cancelled, vec![]),
        }
    }
}

/// Cooperative pause gate that lets `Priority::Immediate` scans preempt
/// background directory workers cleanly between syscalls and resume them once
/// immediate work finishes.
#[derive(Debug, Default)]
pub struct PauseGate {
    state: Mutex<gate_machine::State>,
    cv: Condvar,
}

impl PauseGate {
    pub fn new() -> Self {
        Self::default()
    }

    fn apply(&self, event: gate_machine::Event) {
        let effects = {
            let mut guard = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (next, effects) = gate_machine::step(*guard, event);
            *guard = next;
            effects
        };
        for effect in effects {
            self.perform(effect);
        }
    }

    fn perform(&self, effect: gate_machine::Effect) {
        match effect {
            gate_machine::Effect::WakeAll => self.cv.notify_all(),
        }
    }

    /// Pause background workers while an immediate subtree task runs.
    pub fn pause(&self) {
        self.apply(gate_machine::Event::Pause);
    }

    /// Release one immediate hold; wakes background workers when returning to
    /// `Running`.
    pub fn resume(&self) {
        self.apply(gate_machine::Event::Resume);
    }

    /// Transition to terminal `Cancelled` and wake any parked workers.
    pub fn cancel(&self) {
        self.apply(gate_machine::Event::Cancel);
    }

    /// Wait while in `Paused` (returns immediately on `Running` or `Cancelled`).
    pub fn wait_if_paused(&self, cancel: &AtomicBool) {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while matches!(*guard, gate_machine::State::Paused { .. })
            && !cancel.load(Ordering::Relaxed)
        {
            guard = self
                .cv
                .wait(guard)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        drop(guard);
    }
}

/// Freshness tier of a subdirectory for priority-ordered traversal.
///
/// Scheduling order:
/// 1. `Priority::Immediate` (user-requested, preempts via [`PauseGate`])
/// 2. [`Self::Unscanned`] (no cached data at all — fill blind spots first)
/// 3. [`Self::Stale`] (cached data exists, older than TTL — oldest first, last)
/// 4. [`Self::Fresh`] (within TTL — grafted from cache without touching disk)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirFreshness {
    /// Cached and within TTL: graft without walking disk.
    Fresh,
    /// No complete cached data (missing, stub, or invalidated): walk first.
    Unscanned,
    /// Complete cached data exists, older than TTL: defer and walk oldest-first.
    Stale { scanned_at: u64 },
}

/// Oracle consulted before entering a subdirectory.
///
/// Reuses already-fresh cached (or freshly prioritized) subtrees without
/// touching disk, and classifies remaining subdirectories as unscanned vs stale.
pub trait SkipOracle: Send + Sync {
    /// If `path` is already fresh and compatible, return its ready
    /// [`TreeArena`] to graft directly into the walk.
    fn try_reuse(&self, path: &Path) -> Option<TreeArena>;

    /// Whether this oracle has any fresh subtrees available to reuse. Returning
    /// `false` lets hot loops skip per-directory path construction on cold scans.
    fn has_fresh(&self) -> bool {
        true
    }

    /// Classify `path` for priority ordering: unscanned (no data) walks before
    /// stale (has cached data).
    fn freshness(&self, _path: &Path) -> DirFreshness {
        DirFreshness::Unscanned
    }
}

/// Live atomic counters published while a walk is in progress.
#[derive(Debug, Default)]
pub struct WalkProgress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub allocated_bytes: AtomicU64,
    pub apparent_bytes: AtomicU64,
    pub errors: AtomicU64,
    pub skipped_fresh_dirs: AtomicU64,
    pub walked_dirs: AtomicU64,
    pub active_path: Mutex<Option<PathBuf>>,
    pub messages: Mutex<Vec<String>>,
}

impl WalkProgress {
    pub fn record_error(&self, path: &Path, error: &io::Error) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        let mut messages = self
            .messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if messages.len() < MAX_ERROR_DETAIL {
            messages.push(format!("{}: {error}", path.display()));
        }
        drop(messages);
    }

    pub fn messages(&self) -> Vec<String> {
        self.messages
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn snapshot(&self, duration_ms: u64) -> ScanStats {
        ScanStats {
            files: self.files.load(Ordering::Relaxed),
            dirs: self.dirs.load(Ordering::Relaxed),
            allocated_bytes: self.allocated_bytes.load(Ordering::Relaxed),
            apparent_bytes: self.apparent_bytes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            skipped_fresh_dirs: self.skipped_fresh_dirs.load(Ordering::Relaxed),
            walked_dirs: self.walked_dirs.load(Ordering::Relaxed),
            duration_ms,
        }
    }
}

/// Parameters controlling a single parallel walk.
pub struct WalkConfig<'a> {
    pub options: &'a ScanOptions,
    pub cancel: Arc<AtomicBool>,
    pub progress: Arc<WalkProgress>,
    pub pause_gate: Option<Arc<PauseGate>>,
    pub skip_oracle: Option<Arc<dyn SkipOracle>>,
    /// Optional hook invoked right as a directory begins walking (for live
    /// `SubtreeStatus::Updating` indicators).
    pub on_dir_start: Option<DirStartHook>,
    /// Optional hook invoked after a directory's direct files are tallied (for
    /// live treemap previews during cold scans).
    pub on_dir_tally: Option<DirTallyHook>,
    /// Optional hook invoked when a complete `depth == 1` (or large `depth == 2`)
    /// subtree finishes walking, for incremental checkpointing and live UI
    /// updates without blocking scan workers.
    pub on_dir_complete: Option<DirCompleteHook>,
}

impl std::fmt::Debug for WalkConfig<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalkConfig")
            .field("options", self.options)
            .field("has_pause_gate", &self.pause_gate.is_some())
            .field("has_skip_oracle", &self.skip_oracle.is_some())
            .finish_non_exhaustive()
    }
}

/// Result of a completed parallel walk.
#[derive(Debug)]
pub struct WalkOutcome {
    pub arena: TreeArena,
    pub stats: ScanStats,
    pub scanned_at: u64,
    pub cancelled: bool,
}

/// Choose a balanced worker count from the host's logical CPU count when the
/// caller does not pass an explicit `--scan-threads` override.
///
/// Stays quiet on laptops (4–8 workers on 4–16 logical CPUs) while letting
/// multi-socket / many-core servers scale up to 32 workers.
#[must_use]
pub const fn default_worker_threads(cpus: usize) -> usize {
    if cpus <= 1 {
        1
    } else if cpus <= 4 {
        cpus
    } else if cpus <= 8 {
        4
    } else {
        let half = cpus / 2;
        if half > 32 { 32 } else { half }
    }
}

/// Walk `root` in parallel, grafting any fresh subtrees returned by
/// `config.skip_oracle`, and returning the aggregated, sorted [`TreeArena`].
pub fn walk_parallel(
    root: &Path,
    config: WalkConfig<'_>,
) -> io::Result<WalkOutcome> {
    let started = Instant::now();
    let canonical_root =
        root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let root_meta = fs::metadata(&canonical_root)?;
    if !root_meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", root.display()),
        ));
    }
    let root_dev = device_of(&root_meta);

    let never = never_scanned(&canonical_root);
    let foreign_mounts = if config.options.one_filesystem {
        mount_boundaries_for(&canonical_root)
    } else {
        None
    };
    let mut mount_parents: FxHashSet<PathBuf> = FxHashSet::default();
    for path in &never {
        if let Some(parent) = path.parent() {
            mount_parents.insert(parent.to_path_buf());
        }
    }
    if let Some(mounts) = &foreign_mounts {
        for path in mounts {
            if let Some(parent) = path.parent() {
                mount_parents.insert(parent.to_path_buf());
            }
        }
    }

    let mut visited_dirs = FxHashSet::default();
    if let Some(id) = inode_of(&root_meta) {
        visited_dirs.insert(id);
    }

    let root_name = canonical_root.file_name().map_or_else(
        || canonical_root.to_string_lossy().into_owned(),
        |n| n.to_string_lossy().into_owned(),
    );

    #[cfg(windows)]
    let volume = if !config.options.follow_links {
        crate::windows::walk_volume(&canonical_root)
    } else {
        None
    };

    let ctx = Arc::new(WalkContext {
        options: *config.options,
        #[cfg(target_os = "linux")]
        backend: config.options.backend.resolve(),
        root_dev,
        foreign_mounts,
        never_scanned: never,
        mount_parents,
        visited_dirs: Mutex::new(visited_dirs),
        cancel: config.cancel,
        progress: config.progress,
        pause_gate: config.pause_gate,
        skip_oracle: config.skip_oracle,
        on_dir_start: config.on_dir_start,
        on_dir_tally: config.on_dir_tally,
        on_dir_complete: config.on_dir_complete,
        #[cfg(windows)]
        volume,
        deferred_stale: Mutex::new(Vec::new()),
        finished_root: Mutex::new(None),
    });

    let root_pending = Arc::new(PendingDir::new(
        root_name,
        canonical_root.clone(),
        None,
        0,
        modified_seconds(&root_meta),
        false,
        false,
    ));

    let run_walk = || -> io::Result<()> {
        #[cfg(windows)]
        if ctx.skip_oracle.is_none() {
            if let Some(result) = crate::mft::scan(
                root,
                &canonical_root,
                &ctx.options,
                &ctx.cancel,
                &ctx.progress,
            ) {
                let built = result?;
                *ctx.finished_root
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(built);
                return Ok(());
            }
        }
        let (mut phase, mut effects) = walk_machine::step(
            walk_machine::State::Idle,
            walk_machine::Event::Start,
        );
        while let Some(effect) = effects.pop() {
            let event = perform_walk_effect(&ctx, &root_pending, effect);
            let (next, next_effects) = walk_machine::step(phase, event);
            phase = next;
            effects.extend(next_effects);
        }
        Ok(())
    };

    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
    let threads = config
        .options
        .threads
        .map_or_else(|| default_worker_threads(cpus), |n| n.clamp(1, cpus));
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(io::Error::other)?;
    pool.install(run_walk)?;

    let scanned_at = unix_now();
    let cancelled = ctx.cancel.load(Ordering::Relaxed);
    let built = ctx
        .finished_root
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take()
        .unwrap_or_else(|| BuiltNode::empty_dir(&canonical_root, ""));

    let mut arena = materialize_arena(canonical_root, built, scanned_at);
    arena.aggregate(
        config.options.dedup_hardlinks,
        config.options.include_hidden,
    );
    arena.sort_children(config.options.byte_metric());

    let root_node = arena.node(arena.root());
    let mut stats = ctx.progress.snapshot(started.elapsed().as_millis() as u64);
    stats.files = root_node.files;
    stats.dirs = u64::from(root_node.dirs);
    stats.allocated_bytes = root_node.allocated_bytes;
    stats.apparent_bytes = root_node.apparent_bytes;

    Ok(WalkOutcome {
        arena,
        stats,
        scanned_at,
        cancelled,
    })
}

/// Pure phase scheduler for [`walk_parallel`].
///
/// Ordering: every unscanned (no-data) directory runs first; deferred stale
/// directories run last, sorted oldest `scanned_at` first.
mod walk_machine {
    use std::sync::Arc;

    use super::PendingDir;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum State {
        Idle,
        WalkingUnscanned,
        WalkingStale,
        Done,
    }

    pub enum Event {
        Start,
        PhaseCompleted {
            deferred_stale: Vec<(u64, Arc<PendingDir>)>,
            cancelled: bool,
        },
    }

    pub enum Effect {
        WalkUnscannedRoot,
        WalkStaleBatch(Vec<Arc<PendingDir>>),
    }

    pub fn step(state: State, event: Event) -> (State, Vec<Effect>) {
        match (state, event) {
            (State::Idle, Event::Start) => {
                (State::WalkingUnscanned, vec![Effect::WalkUnscannedRoot])
            }
            (
                State::WalkingUnscanned | State::WalkingStale,
                Event::PhaseCompleted {
                    mut deferred_stale,
                    cancelled,
                },
            ) => {
                if cancelled || deferred_stale.is_empty() {
                    return (State::Done, vec![]);
                }
                // Oldest cached data refreshes first.
                deferred_stale.sort_by_key(|(scanned_at, _)| *scanned_at);
                let ordered =
                    deferred_stale.into_iter().map(|(_, dir)| dir).collect();
                (State::WalkingStale, vec![Effect::WalkStaleBatch(ordered)])
            }
            (
                state @ (State::WalkingUnscanned
                | State::WalkingStale
                | State::Done),
                Event::Start,
            )
            | (
                state @ (State::Idle | State::Done),
                Event::PhaseCompleted { .. },
            ) => (state, vec![]),
        }
    }
}

fn perform_walk_effect(
    ctx: &Arc<WalkContext>,
    root_pending: &Arc<PendingDir>,
    effect: walk_machine::Effect,
) -> walk_machine::Event {
    match effect {
        walk_machine::Effect::WalkUnscannedRoot => {
            rayon::scope(|scope| {
                let ctx = Arc::clone(ctx);
                let root_pending = Arc::clone(root_pending);
                scope.spawn(move |s| {
                    scan_dir(s, &ctx, &root_pending);
                });
            });
        }
        walk_machine::Effect::WalkStaleBatch(batch) => {
            // `scope_fifo` preserves oldest-first spawn order rather than
            // Rayon's default LIFO deque.
            rayon::scope_fifo(|scope| {
                for sub in batch {
                    let ctx = Arc::clone(ctx);
                    scope.spawn_fifo(move |_| {
                        run_deferred_stale(&ctx, &sub);
                    });
                }
            });
        }
    }
    let cancelled = ctx.cancel.load(Ordering::Relaxed);
    let deferred_stale = std::mem::take(
        &mut *ctx
            .deferred_stale
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    walk_machine::Event::PhaseCompleted {
        deferred_stale,
        cancelled,
    }
}

fn run_deferred_stale(ctx: &Arc<WalkContext>, sub: &Arc<PendingDir>) {
    // A user may have prioritized this stale subtree while unscanned
    // directories were walking: graft it without walking twice.
    if let Some(oracle) = &ctx.skip_oracle
        && let Some(reused) = oracle.try_reuse(&sub.path)
    {
        let reused_root = reused.node(reused.root());
        ctx.progress
            .skipped_fresh_dirs
            .fetch_add(u64::from(reused_root.dirs), Ordering::Relaxed);
        ctx.progress
            .dirs
            .fetch_add(u64::from(reused_root.dirs), Ordering::Relaxed);
        ctx.progress
            .files
            .fetch_add(reused_root.files, Ordering::Relaxed);
        ctx.progress
            .allocated_bytes
            .fetch_add(reused_root.allocated_bytes, Ordering::Relaxed);
        ctx.progress
            .apparent_bytes
            .fetch_add(reused_root.apparent_bytes, Ordering::Relaxed);
        if let Some(parent) = &sub.parent {
            parent
                .children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(BuiltChild::Grafted(Box::new(GraftedChild {
                    name: sub.name.clone(),
                    arena: Arc::new(reused),
                    hidden: sub.hidden,
                    lossy_name: sub.lossy_name,
                })));
            complete_one(ctx, parent);
        }
        return;
    }
    rayon::scope(|inner| {
        scan_dir(inner, ctx, sub);
    });
}

struct WalkContext {
    options: ScanOptions,
    #[cfg(target_os = "linux")]
    backend: crate::backend::ScanBackend,
    root_dev: u64,
    foreign_mounts: Option<FxHashSet<PathBuf>>,
    never_scanned: FxHashSet<PathBuf>,
    /// Parent directories of every path in `never_scanned` / `foreign_mounts`.
    /// Directories absent from this set skip all per-entry mount hash checks.
    mount_parents: FxHashSet<PathBuf>,
    visited_dirs: Mutex<FxHashSet<(u64, u64)>>,
    cancel: Arc<AtomicBool>,
    progress: Arc<WalkProgress>,
    pause_gate: Option<Arc<PauseGate>>,
    skip_oracle: Option<Arc<dyn SkipOracle>>,
    on_dir_start: Option<DirStartHook>,
    on_dir_tally: Option<DirTallyHook>,
    on_dir_complete: Option<DirCompleteHook>,
    #[cfg(windows)]
    volume: Option<u64>,
    /// Stale subdirectories (already have cached data) deferred until every
    /// Unscanned (no-data) subdirectory finishes.
    deferred_stale: Mutex<Vec<(u64, Arc<PendingDir>)>>,
    finished_root: Mutex<Option<BuiltNode>>,
}

/// Intermediate directory accumulator using the `+1` completion sentinel
/// (Invariant #3): never built until its own listing AND every child task finish.
struct PendingDir {
    name: String,
    path: PathBuf,
    parent: Option<Arc<Self>>,
    depth: usize,
    modified: i64,
    hidden: bool,
    lossy_name: bool,
    read_error: AtomicBool,
    /// Set if this directory or any descendant was cut short by cancellation.
    /// Incomplete subtrees are never incrementally checkpointed to the DB.
    incomplete: AtomicBool,
    /// Starts at `1` (the `+1` sentinel for this directory's own scan).
    pending: AtomicUsize,
    leaves: Mutex<LeafSlab>,
    children: Mutex<Vec<BuiltChild>>,
}

impl PendingDir {
    const fn new(
        name: String,
        path: PathBuf,
        parent: Option<Arc<Self>>,
        depth: usize,
        modified: i64,
        hidden: bool,
        lossy_name: bool,
    ) -> Self {
        Self {
            name,
            path,
            parent,
            depth,
            modified,
            hidden,
            lossy_name,
            read_error: AtomicBool::new(false),
            incomplete: AtomicBool::new(false),
            pending: AtomicUsize::new(1),
            leaves: Mutex::new(LeafSlab::new()),
            children: Mutex::new(Vec::new()),
        }
    }
}

/// Struct-of-Arrays (`SoA`) bump slab for a directory's direct leaf entries.
///
/// Zero per-file heap allocations: all UTF-8 names sit back-to-back in `names`,
/// numeric fields live in contiguous parallel arrays, and hardlinks are stored
/// sparsely (only files with `nlink > 1`).
#[derive(Default)]
pub(crate) struct LeafSlab {
    names: Vec<u8>,
    name_ends: Vec<u32>,
    allocated: Vec<u64>,
    apparent: Vec<u64>,
    modified: Vec<i64>,
    /// Packed `kind: 2 bits | hidden: bit 2 | lossy: bit 3`.
    meta: Vec<u8>,
    /// Sparse `(leaf_index, dev, ino)` for multi-link files.
    hardlinks: Vec<(u32, u64, u64)>,
}

impl LeafSlab {
    const fn new() -> Self {
        Self {
            names: Vec::new(),
            name_ends: Vec::new(),
            allocated: Vec::new(),
            apparent: Vec::new(),
            modified: Vec::new(),
            meta: Vec::new(),
            hardlinks: Vec::new(),
        }
    }

    const fn len(&self) -> usize {
        self.meta.len()
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "flat SoA push avoids intermediate struct copies"
    )]
    fn push(
        &mut self,
        name: &str,
        kind: NodeKind,
        allocated_bytes: u64,
        apparent_bytes: u64,
        modified: i64,
        hidden: bool,
        lossy_name: bool,
        hardlink: Option<(u64, u64)>,
    ) {
        let index = self.meta.len() as u32;
        self.names.extend_from_slice(name.as_bytes());
        self.name_ends.push(self.names.len() as u32);
        self.allocated.push(allocated_bytes);
        self.apparent.push(apparent_bytes);
        self.modified.push(modified);
        let kind_bits = match kind {
            NodeKind::Directory => 0u8,
            NodeKind::File => 1u8,
            NodeKind::Symlink => 2u8,
            NodeKind::Other => 3u8,
        };
        let packed =
            kind_bits | (u8::from(hidden) << 2) | (u8::from(lossy_name) << 3);
        self.meta.push(packed);
        if let Some((dev, ino)) = hardlink {
            self.hardlinks.push((index, dev, ino));
        }
    }

    const fn unpack_meta(packed: u8) -> (NodeKind, bool, bool) {
        let kind = match packed & 0b11 {
            0 => NodeKind::Directory,
            1 => NodeKind::File,
            2 => NodeKind::Symlink,
            _ => NodeKind::Other,
        };
        let hidden = (packed & (1 << 2)) != 0;
        let lossy = (packed & (1 << 3)) != 0;
        (kind, hidden, lossy)
    }
}

pub(crate) enum BuiltChild {
    #[cfg_attr(not(windows), allow(dead_code, reason = "used by Windows MFT"))]
    Leaf(LeafRecord),
    Dir(Box<BuiltNode>),
    Grafted(Box<GraftedChild>),
}

pub(crate) struct GraftedChild {
    pub name: String,
    pub arena: Arc<TreeArena>,
    pub hidden: bool,
    pub lossy_name: bool,
}

#[cfg_attr(not(windows), allow(dead_code, reason = "used by Windows MFT"))]
pub(crate) struct LeafRecord {
    pub name: Box<str>,
    pub kind: NodeKind,
    pub allocated_bytes: u64,
    pub apparent_bytes: u64,
    pub modified: i64,
    pub hidden: bool,
    pub lossy_name: bool,
    pub hardlink: Option<Box<(u64, u64)>>,
}

pub(crate) struct BuiltNode {
    pub name: String,
    pub modified: i64,
    pub read_error: bool,
    pub hidden: bool,
    pub lossy_name: bool,
    pub leaves: LeafSlab,
    pub children: Vec<BuiltChild>,
}

impl BuiltNode {
    fn empty_dir(path: &Path, fallback_name: &str) -> Self {
        let name = path.file_name().map_or_else(
            || fallback_name.to_owned(),
            |n| n.to_string_lossy().into_owned(),
        );
        Self {
            name,
            modified: 0,
            read_error: false,
            hidden: false,
            lossy_name: false,
            leaves: LeafSlab::new(),
            children: Vec::new(),
        }
    }
}

#[cfg(not(target_os = "linux"))]
struct DirFacts {
    device: u64,
    modified: i64,
    evicted: bool,
    inode: Option<(u64, u64)>,
}

#[cfg(not(target_os = "linux"))]
struct LeafFacts {
    allocated_bytes: u64,
    apparent_bytes: u64,
    modified: i64,
    hardlink: Option<(u64, u64)>,
}

#[cfg(not(target_os = "linux"))]
trait Listed {
    fn file_name_parts(&self) -> (String, bool);
    fn entry_path(&self, dir: &Path) -> PathBuf;
    fn is_hidden(&self, name: &str) -> bool {
        name.starts_with('.')
    }
    fn kind(&self) -> io::Result<NodeKind>;
    fn dir_facts(&self) -> io::Result<DirFacts>;
    fn leaf_facts(&self, follow_links: bool) -> io::Result<LeafFacts>;
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
impl Listed for fs::DirEntry {
    fn file_name_parts(&self) -> (String, bool) {
        let raw = self.file_name();
        let lossy = raw.to_str().is_none();
        (raw.to_string_lossy().into_owned(), lossy)
    }

    fn entry_path(&self, _dir: &Path) -> PathBuf {
        self.path()
    }

    fn kind(&self) -> io::Result<NodeKind> {
        let ft = self.file_type()?;
        Ok(if ft.is_symlink() {
            NodeKind::Symlink
        } else if ft.is_dir() {
            NodeKind::Directory
        } else if ft.is_file() {
            NodeKind::File
        } else {
            NodeKind::Other
        })
    }

    fn dir_facts(&self) -> io::Result<DirFacts> {
        let meta = self.metadata()?;
        Ok(DirFacts {
            device: device_of(&meta),
            modified: modified_seconds(&meta),
            evicted: false,
            inode: inode_of(&meta),
        })
    }

    fn leaf_facts(&self, follow_links: bool) -> io::Result<LeafFacts> {
        let meta = self.metadata()?;
        let (allocated_bytes, apparent_bytes) = measure_both(&meta);
        let hardlink = if follow_links || nlink_of(&meta) > 1 {
            inode_of(&meta)
        } else {
            None
        };
        Ok(LeafFacts {
            allocated_bytes,
            apparent_bytes,
            modified: modified_seconds(&meta),
            hardlink,
        })
    }
}

#[cfg(target_os = "macos")]
impl Listed for crate::macos::Entry {
    fn file_name_parts(&self) -> (String, bool) {
        let lossy = self.file_name().to_str().is_none();
        (self.name().to_owned(), lossy)
    }

    fn entry_path(&self, dir: &Path) -> PathBuf {
        self.path(dir)
    }

    fn kind(&self) -> io::Result<NodeKind> {
        Ok(match self.kind() {
            crate::macos::Kind::Link => NodeKind::Symlink,
            crate::macos::Kind::Directory => NodeKind::Directory,
            crate::macos::Kind::File => NodeKind::File,
            crate::macos::Kind::Other => NodeKind::Other,
        })
    }

    fn dir_facts(&self) -> io::Result<DirFacts> {
        Ok(DirFacts {
            device: self.device(),
            modified: self.modified(),
            evicted: self.evicted(),
            inode: Some(self.identity()),
        })
    }

    fn leaf_facts(&self, follow_links: bool) -> io::Result<LeafFacts> {
        Ok(LeafFacts {
            allocated_bytes: self.allocated(),
            apparent_bytes: self.apparent(),
            modified: self.modified(),
            hardlink: (follow_links || self.shared()).then(|| self.identity()),
        })
    }
}

#[cfg(windows)]
impl Listed for crate::windows::Entry {
    fn file_name_parts(&self) -> (String, bool) {
        (self.name().to_owned(), false)
    }

    fn entry_path(&self, dir: &Path) -> PathBuf {
        self.path(dir)
    }

    fn is_hidden(&self, name: &str) -> bool {
        name.starts_with('.') || self.hidden()
    }

    fn kind(&self) -> io::Result<NodeKind> {
        Ok(match self.kind() {
            crate::windows::Kind::Link => NodeKind::Symlink,
            crate::windows::Kind::Directory => NodeKind::Directory,
            crate::windows::Kind::File => NodeKind::File,
        })
    }

    fn dir_facts(&self) -> io::Result<DirFacts> {
        Ok(DirFacts {
            device: 0,
            modified: self.modified(),
            evicted: self.evicted(),
            inode: self.identity(),
        })
    }

    fn leaf_facts(&self, _follow_links: bool) -> io::Result<LeafFacts> {
        Ok(LeafFacts {
            allocated_bytes: self.allocated(),
            apparent_bytes: self.apparent(),
            modified: self.modified(),
            hardlink: self.identity(),
        })
    }
}

#[cfg(target_os = "macos")]
fn read_dir_entries(
    path: &Path,
    _ctx: &WalkContext,
) -> io::Result<crate::macos::ReadDir> {
    crate::macos::read_dir(path)
}

#[cfg(windows)]
fn read_dir_entries(
    path: &Path,
    ctx: &WalkContext,
) -> io::Result<crate::windows::ReadDir> {
    crate::windows::read_dir(path, ctx.volume)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn read_dir_entries(
    path: &Path,
    _ctx: &WalkContext,
) -> io::Result<fs::ReadDir> {
    fs::read_dir(path)
}

/// Depth at which Linux workers switch from Rayon fan-out to a zero-allocation
/// iterative subtree walker (`walk_subtree_direct`), keeping all cores
/// saturated across shallow branches (e.g. `.cargo/registry/src/<idx>/<crate>`
/// and `.bun/install/cache/<pkg>`) while eliminating per-directory `Arc`/
/// `Mutex`/`PathBuf` allocations across the 95%+ deeper directories.
#[cfg(target_os = "linux")]
const DIRECT_SUBTREE_DEPTH: usize = 4;

#[cfg(target_os = "linux")]
fn scan_dir(scope: &Scope<'_>, ctx: &Arc<WalkContext>, dir: &Arc<PendingDir>) {
    if dir.depth >= DIRECT_SUBTREE_DEPTH {
        scan_dir_linux_direct(ctx, dir);
    } else {
        scan_dir_linux(scope, ctx, dir);
    }
}

#[cfg(target_os = "linux")]
#[allow(
    clippy::too_many_lines,
    reason = "strategy-invariant zero-alloc iterative subtree walker"
)]
fn scan_dir_linux_direct(ctx: &Arc<WalkContext>, dir: &Arc<PendingDir>) {
    use std::ffi::OsStr;
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};

    // Single reusable PathBuf + flat LIFO byte buffer for child dir names:
    // zero PathBuf / String / Arc / Mutex / PendingDir allocations for the
    // entire subtree beneath `dir`.
    struct Frame {
        node_id: NodeId,
        depth: usize,
        path_len: usize,
        name_start: usize,
        name_end: usize,
    }

    if let Some(gate) = &ctx.pause_gate {
        gate.wait_if_paused(&ctx.cancel);
    }
    if ctx.cancel.load(Ordering::Relaxed) {
        dir.incomplete.store(true, Ordering::Relaxed);
        complete_one(ctx, dir);
        return;
    }

    let scanned_at = unix_now();
    let mut arena = TreeArena::new(dir.path.clone(), &dir.name);
    let sub_root_id = arena.root();
    {
        let r = arena.node_mut(sub_root_id);
        r.modified = dir.modified;
        r.flags.set(NodeFlags::HIDDEN, dir.hidden);
        r.flags.set(NodeFlags::LOSSY_NAME, dir.lossy_name);
        r.set_status(SubtreeStatus::Fresh { scanned_at });
    }

    let mut cur_path = dir.path.clone().into_os_string();
    let mut name_buf: Vec<u8> = Vec::with_capacity(1024);
    let mut stack: Vec<Frame> = Vec::with_capacity(64);
    stack.push(Frame {
        node_id: sub_root_id,
        depth: dir.depth,
        path_len: cur_path.len(),
        name_start: 0,
        name_end: 0,
    });

    let mut incomplete = false;
    let mut batch_dirs = 0u64;
    let mut batch_files = 0u64;
    let mut batch_alloc = 0u64;
    let mut batch_apparent = 0u64;
    let mut batch_errors = 0u64;

    let flush_batch = |batch_dirs: &mut u64,
                       batch_files: &mut u64,
                       batch_alloc: &mut u64,
                       batch_apparent: &mut u64,
                       batch_errors: &mut u64| {
        if *batch_dirs > 0 {
            ctx.progress
                .walked_dirs
                .fetch_add(*batch_dirs, Ordering::Relaxed);
            ctx.progress.dirs.fetch_add(*batch_dirs, Ordering::Relaxed);
            *batch_dirs = 0;
        }
        if *batch_files > 0 {
            ctx.progress
                .files
                .fetch_add(*batch_files, Ordering::Relaxed);
            *batch_files = 0;
        }
        if *batch_alloc > 0 {
            ctx.progress
                .allocated_bytes
                .fetch_add(*batch_alloc, Ordering::Relaxed);
            *batch_alloc = 0;
        }
        if *batch_apparent > 0 {
            ctx.progress
                .apparent_bytes
                .fetch_add(*batch_apparent, Ordering::Relaxed);
            *batch_apparent = 0;
        }
        if *batch_errors > 0 {
            ctx.progress
                .errors
                .fetch_add(*batch_errors, Ordering::Relaxed);
            *batch_errors = 0;
        }
    };

    while let Some(frame) = stack.pop() {
        if let Some(gate) = &ctx.pause_gate {
            gate.wait_if_paused(&ctx.cancel);
        }
        if ctx.cancel.load(Ordering::Relaxed) {
            incomplete = true;
            break;
        }

        // Reconstruct `cur_path` in-place with zero heap allocation.
        if frame.name_end > frame.name_start {
            let mut bytes = cur_path.into_vec();
            bytes.truncate(frame.path_len);
            if !bytes.ends_with(b"/") {
                bytes.push(b'/');
            }
            bytes
                .extend_from_slice(&name_buf[frame.name_start..frame.name_end]);
            name_buf.truncate(frame.name_start);
            cur_path = std::ffi::OsString::from_vec(bytes);
        }

        batch_dirs += 1;
        if batch_dirs >= 64 {
            flush_batch(
                &mut batch_dirs,
                &mut batch_files,
                &mut batch_alloc,
                &mut batch_apparent,
                &mut batch_errors,
            );
            if let Ok(mut active) = ctx.progress.active_path.try_lock() {
                *active = Some(PathBuf::from(&cur_path));
            }
        }

        let dir_path_ref = Path::new(&cur_path);
        let check_mounts = !ctx.mount_parents.is_empty()
            && ctx.mount_parents.contains(dir_path_ref);
        let check_oracle =
            ctx.skip_oracle.as_ref().is_some_and(|o| o.has_fresh());
        let parent_path_len = cur_path.len();
        let mut dir_files = 0u64;
        let mut dir_alloc = 0u64;
        let mut dir_apparent = 0u64;
        let mut deferred_grafts: Vec<(Vec<u8>, TreeArena)> = Vec::new();

        let read_res = crate::backend::visit_dir_entries(
            dir_path_ref,
            ctx.options.follow_links,
            ctx.backend,
            |raw_bytes, is_dir_hint| {
                if check_mounts {
                    let path = dir_path_ref.join(OsStr::from_bytes(raw_bytes));
                    if ctx.never_scanned.contains(&path)
                        || (ctx.options.one_filesystem
                            && ctx
                                .foreign_mounts
                                .as_ref()
                                .is_some_and(|m| m.contains(&path)))
                    {
                        return true;
                    }
                }
                if is_dir_hint
                    && check_oracle
                    && let Some(oracle) = &ctx.skip_oracle
                {
                    let path = dir_path_ref.join(OsStr::from_bytes(raw_bytes));
                    if let Some(reused) = oracle.try_reuse(&path) {
                        deferred_grafts.push((raw_bytes.to_vec(), reused));
                        return true;
                    }
                }
                false
            },
            |raw_bytes, meta_res| {
                if ctx.cancel.load(Ordering::Relaxed) {
                    incomplete = true;
                    return false;
                }
                let st = match meta_res {
                    Ok(st) => st,
                    Err(err) => {
                        if !(ctx.options.follow_links
                            && err.kind() == io::ErrorKind::NotFound)
                        {
                            batch_errors += 1;
                        }
                        return true;
                    }
                };
                let hidden = st.hidden_attr || raw_bytes.first() == Some(&b'.');
                let utf8 = std::str::from_utf8(raw_bytes);
                let lossy_name = utf8.is_err();
                let lossy_owned;
                let name_str: &str = if let Ok(s) = utf8 {
                    s
                } else {
                    lossy_owned = String::from_utf8_lossy(raw_bytes);
                    lossy_owned.as_ref()
                };

                if st.kind == NodeKind::Directory {
                    if st.evicted {
                        return true;
                    }
                    if ctx.options.one_filesystem
                        && ctx.foreign_mounts.is_none()
                        && st.device != ctx.root_dev
                    {
                        return true;
                    }
                    if ctx.options.follow_links
                        && let Some(id) = st.inode
                        && !ctx
                            .visited_dirs
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(id)
                    {
                        return true;
                    }
                    if ctx
                        .options
                        .max_depth
                        .is_some_and(|limit| frame.depth + 1 > limit)
                    {
                        return true;
                    }
                    let mut flags = NodeFlags::new(NodeKind::Directory);
                    flags.set(NodeFlags::HIDDEN, hidden);
                    flags.set(NodeFlags::LOSSY_NAME, lossy_name);
                    let cid = arena.add_child_raw(
                        frame.node_id,
                        name_str,
                        NodeKind::Directory,
                        0,
                        0,
                        st.modified,
                        flags,
                    );
                    arena
                        .node_mut(cid)
                        .set_status(SubtreeStatus::Fresh { scanned_at });
                    let name_start = name_buf.len();
                    name_buf.extend_from_slice(raw_bytes);
                    let name_end = name_buf.len();
                    stack.push(Frame {
                        node_id: cid,
                        depth: frame.depth + 1,
                        path_len: parent_path_len,
                        name_start,
                        name_end,
                    });
                    return true;
                }

                let (alloc, apparent) = if st.kind == NodeKind::Other {
                    (0, 0)
                } else {
                    (st.allocated_bytes, st.apparent_bytes)
                };
                if st.kind == NodeKind::File {
                    dir_files += 1;
                }
                dir_alloc = dir_alloc.saturating_add(alloc);
                dir_apparent = dir_apparent.saturating_add(apparent);
                let mut flags = NodeFlags::new(st.kind);
                flags.set(NodeFlags::HIDDEN, hidden);
                flags.set(NodeFlags::LOSSY_NAME, lossy_name);
                let id = arena.add_child_raw(
                    frame.node_id,
                    name_str,
                    st.kind,
                    alloc,
                    apparent,
                    st.modified,
                    flags,
                );
                if st.kind == NodeKind::File
                    && let Some((dev, ino)) = st.hardlink
                {
                    arena.record_hardlink(id, dev, ino);
                }
                true
            },
        );

        for (raw_bytes, reused) in deferred_grafts {
            let hidden = raw_bytes.first() == Some(&b'.');
            let utf8 = std::str::from_utf8(&raw_bytes);
            let lossy_name = utf8.is_err();
            let lossy_owned;
            let name_str: &str = if let Ok(s) = utf8 {
                s
            } else {
                lossy_owned = String::from_utf8_lossy(&raw_bytes);
                lossy_owned.as_ref()
            };
            let reused_root = reused.node(reused.root());
            ctx.progress
                .skipped_fresh_dirs
                .fetch_add(u64::from(reused_root.dirs), Ordering::Relaxed);
            batch_dirs += u64::from(reused_root.dirs);
            batch_files += reused_root.files;
            batch_alloc =
                batch_alloc.saturating_add(reused_root.allocated_bytes);
            batch_apparent =
                batch_apparent.saturating_add(reused_root.apparent_bytes);
            if let Some(hook) = &ctx.on_dir_tally {
                let path = dir_path_ref.join(OsStr::from_bytes(&raw_bytes));
                hook(
                    &path,
                    reused_root.allocated_bytes,
                    reused_root.apparent_bytes,
                    reused_root.files,
                );
            }
            let gid =
                arena.graft_subtree(frame.node_id, &reused, Some(name_str));
            let gn = arena.node_mut(gid);
            gn.flags.set(NodeFlags::HIDDEN, hidden);
            gn.flags.set(NodeFlags::LOSSY_NAME, lossy_name);
        }

        if let Err(error) = read_res {
            arena
                .node_mut(frame.node_id)
                .flags
                .set(NodeFlags::READ_ERROR, true);
            ctx.progress.record_error(dir_path_ref, &error);
        }

        if dir_files > 0 || dir_alloc > 0 || dir_apparent > 0 {
            batch_files += dir_files;
            batch_alloc = batch_alloc.saturating_add(dir_alloc);
            batch_apparent = batch_apparent.saturating_add(dir_apparent);
            if let Some(hook) = &ctx.on_dir_tally {
                hook(dir_path_ref, dir_alloc, dir_apparent, dir_files);
            }
        }
    }

    flush_batch(
        &mut batch_dirs,
        &mut batch_files,
        &mut batch_alloc,
        &mut batch_apparent,
        &mut batch_errors,
    );

    if incomplete {
        dir.incomplete.store(true, Ordering::Relaxed);
    }

    // Attach the directly built `TreeArena` onto `dir.parent` and release `dir`
    // cleanly (no `BuiltNode`, no `LeafSlab`, no `materialize_arena` copy!).
    if let Some(parent) = dir.parent.as_ref() {
        if incomplete {
            parent.incomplete.store(true, Ordering::Relaxed);
        }
        parent
            .children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(BuiltChild::Grafted(Box::new(GraftedChild {
                name: dir.name.clone(),
                arena: Arc::new(arena),
                hidden: dir.hidden,
                lossy_name: dir.lossy_name,
            })));
        complete_one(ctx, parent);
    }
}

#[cfg(target_os = "linux")]
#[allow(
    clippy::too_many_lines,
    reason = "strategy-invariant shallow fan-out directory scan"
)]
fn scan_dir_linux(
    scope: &Scope<'_>,
    ctx: &Arc<WalkContext>,
    dir: &Arc<PendingDir>,
) {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    if let Some(gate) = &ctx.pause_gate {
        gate.wait_if_paused(&ctx.cancel);
    }

    if ctx.cancel.load(Ordering::Relaxed) {
        dir.incomplete.store(true, Ordering::Relaxed);
        complete_one(ctx, dir);
        return;
    }

    ctx.progress.walked_dirs.fetch_add(1, Ordering::Relaxed);
    ctx.progress.dirs.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut active) = ctx.progress.active_path.try_lock() {
        *active = Some(dir.path.clone());
    }
    if let Some(hook) = &ctx.on_dir_start {
        hook(&dir.path);
    }

    let check_mounts = ctx.mount_parents.contains(&dir.path);
    let check_oracle = ctx.skip_oracle.as_ref().is_some_and(|o| o.has_fresh());
    let mut local_leaves = LeafSlab::new();
    let mut local_children: Vec<BuiltChild> = Vec::new();
    let mut subdirs: Vec<Arc<PendingDir>> = Vec::new();
    let mut local_files = 0u64;
    let mut local_alloc = 0u64;
    let mut local_apparent = 0u64;

    let read_res = crate::backend::visit_dir_entries(
        &dir.path,
        ctx.options.follow_links,
        ctx.backend,
        |raw_bytes, is_dir_hint| {
            if check_mounts {
                let path = dir.path.join(OsStr::from_bytes(raw_bytes));
                if ctx.never_scanned.contains(&path)
                    || (ctx.options.one_filesystem
                        && ctx
                            .foreign_mounts
                            .as_ref()
                            .is_some_and(|m| m.contains(&path)))
                {
                    return true;
                }
            }
            if is_dir_hint
                && check_oracle
                && let Some(oracle) = &ctx.skip_oracle
            {
                let path = dir.path.join(OsStr::from_bytes(raw_bytes));
                if let Some(reused) = oracle.try_reuse(&path) {
                    let hidden = raw_bytes.first() == Some(&b'.');
                    let utf8 = std::str::from_utf8(raw_bytes);
                    let lossy_name = utf8.is_err();
                    let name = utf8.map_or_else(
                        |_| String::from_utf8_lossy(raw_bytes).into_owned(),
                        str::to_owned,
                    );
                    let reused_root = reused.node(reused.root());
                    ctx.progress.skipped_fresh_dirs.fetch_add(
                        u64::from(reused_root.dirs),
                        Ordering::Relaxed,
                    );
                    ctx.progress.dirs.fetch_add(
                        u64::from(reused_root.dirs),
                        Ordering::Relaxed,
                    );
                    ctx.progress
                        .files
                        .fetch_add(reused_root.files, Ordering::Relaxed);
                    ctx.progress.allocated_bytes.fetch_add(
                        reused_root.allocated_bytes,
                        Ordering::Relaxed,
                    );
                    ctx.progress.apparent_bytes.fetch_add(
                        reused_root.apparent_bytes,
                        Ordering::Relaxed,
                    );
                    if let Some(hook) = &ctx.on_dir_tally {
                        hook(
                            &path,
                            reused_root.allocated_bytes,
                            reused_root.apparent_bytes,
                            reused_root.files,
                        );
                    }
                    local_children.push(BuiltChild::Grafted(Box::new(
                        GraftedChild {
                            name,
                            arena: Arc::new(reused),
                            hidden,
                            lossy_name,
                        },
                    )));
                    return true;
                }
            }
            false
        },
        |raw_bytes, meta_res| {
            if ctx.cancel.load(Ordering::Relaxed) {
                dir.incomplete.store(true, Ordering::Relaxed);
                return false;
            }
            let st = match meta_res {
                Ok(st) => st,
                Err(err) => {
                    if !(ctx.options.follow_links
                        && err.kind() == io::ErrorKind::NotFound)
                    {
                        ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
                    }
                    return true;
                }
            };

            let hidden = st.hidden_attr || raw_bytes.first() == Some(&b'.');
            let utf8 = std::str::from_utf8(raw_bytes);
            let lossy_name = utf8.is_err();
            let lossy_owned;
            let name_str: &str = if let Ok(s) = utf8 {
                s
            } else {
                lossy_owned = String::from_utf8_lossy(raw_bytes);
                lossy_owned.as_ref()
            };

            if st.kind == NodeKind::Directory {
                if st.evicted {
                    return true;
                }
                if ctx.options.one_filesystem
                    && ctx.foreign_mounts.is_none()
                    && st.device != ctx.root_dev
                {
                    return true;
                }
                if ctx.options.follow_links
                    && let Some(id) = st.inode
                    && !ctx
                        .visited_dirs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(id)
                {
                    return true;
                }
                if ctx
                    .options
                    .max_depth
                    .is_some_and(|limit| dir.depth + 1 > limit)
                {
                    return true;
                }
                let path = dir.path.join(OsStr::from_bytes(raw_bytes));
                subdirs.push(Arc::new(PendingDir::new(
                    name_str.to_owned(),
                    path,
                    Some(Arc::clone(dir)),
                    dir.depth + 1,
                    st.modified,
                    hidden,
                    lossy_name,
                )));
                return true;
            }

            let (alloc, apparent) = if st.kind == NodeKind::Other {
                (0, 0)
            } else {
                (st.allocated_bytes, st.apparent_bytes)
            };
            if st.kind == NodeKind::File {
                local_files += 1;
            }
            local_alloc = local_alloc.saturating_add(alloc);
            local_apparent = local_apparent.saturating_add(apparent);
            local_leaves.push(
                name_str,
                st.kind,
                alloc,
                apparent,
                st.modified,
                hidden,
                lossy_name,
                (st.kind == NodeKind::File).then_some(st.hardlink).flatten(),
            );
            true
        },
    );

    if let Err(error) = read_res {
        dir.read_error.store(true, Ordering::Relaxed);
        ctx.progress.record_error(&dir.path, &error);
        complete_one(ctx, dir);
        return;
    }

    if local_files > 0 || local_alloc > 0 || local_apparent > 0 {
        ctx.progress.files.fetch_add(local_files, Ordering::Relaxed);
        ctx.progress
            .allocated_bytes
            .fetch_add(local_alloc, Ordering::Relaxed);
        ctx.progress
            .apparent_bytes
            .fetch_add(local_apparent, Ordering::Relaxed);
        if let Some(hook) = &ctx.on_dir_tally {
            hook(&dir.path, local_alloc, local_apparent, local_files);
        }
    }

    if local_leaves.len() > 0 {
        *dir.leaves
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = local_leaves;
    }
    if !local_children.is_empty() {
        dir.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(local_children);
    }

    if !subdirs.is_empty() {
        dir.pending.fetch_add(subdirs.len(), Ordering::AcqRel);
        let mut stale_deferred: Vec<(u64, Arc<PendingDir>)> = Vec::new();
        for sub in subdirs {
            if let Some(oracle) = &ctx.skip_oracle
                && let DirFreshness::Stale { scanned_at } =
                    oracle.freshness(&sub.path)
            {
                stale_deferred.push((scanned_at, sub));
                continue;
            }
            let ctx = Arc::clone(ctx);
            scope.spawn(move |s| {
                scan_dir(s, &ctx, &sub);
            });
        }
        if !stale_deferred.is_empty() {
            ctx.deferred_stale
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(stale_deferred);
        }
    }

    complete_one(ctx, dir);
}

#[cfg(not(target_os = "linux"))]
fn scan_dir(scope: &Scope<'_>, ctx: &Arc<WalkContext>, dir: &Arc<PendingDir>) {
    if let Some(gate) = &ctx.pause_gate {
        gate.wait_if_paused(&ctx.cancel);
    }

    if ctx.cancel.load(Ordering::Relaxed) {
        dir.incomplete.store(true, Ordering::Relaxed);
        complete_one(ctx, dir);
        return;
    }

    ctx.progress.walked_dirs.fetch_add(1, Ordering::Relaxed);
    ctx.progress.dirs.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut active) = ctx.progress.active_path.try_lock() {
        *active = Some(dir.path.clone());
    }
    if let Some(hook) = &ctx.on_dir_start {
        hook(&dir.path);
    }

    let entries = match read_dir_entries(&dir.path, ctx) {
        Ok(entries) => entries,
        Err(error) => {
            dir.read_error.store(true, Ordering::Relaxed);
            ctx.progress.record_error(&dir.path, &error);
            complete_one(ctx, dir);
            return;
        }
    };

    let mut local_children: Vec<BuiltChild> = Vec::new();
    let mut subdirs: Vec<Arc<PendingDir>> = Vec::new();
    let mut local_files = 0u64;
    let mut local_alloc = 0u64;
    let mut local_apparent = 0u64;

    for entry_res in entries {
        if ctx.cancel.load(Ordering::Relaxed) {
            dir.incomplete.store(true, Ordering::Relaxed);
            break;
        }
        let entry = match entry_res {
            Ok(entry) => entry,
            Err(error) => {
                dir.read_error.store(true, Ordering::Relaxed);
                ctx.progress.record_error(&dir.path, &error);
                continue;
            }
        };

        let (name, lossy_name) = entry.file_name_parts();
        let hidden = entry.is_hidden(&name);
        let path = entry.entry_path(&dir.path);

        let Ok(kind) = Listed::kind(&entry) else {
            ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };

        if kind == NodeKind::Symlink {
            if !ctx.options.follow_links {
                if let Ok(meta) = fs::symlink_metadata(&path) {
                    let (alloc, apparent) = measure_both(&meta);
                    local_alloc = local_alloc.saturating_add(alloc);
                    local_apparent = local_apparent.saturating_add(apparent);
                    local_children.push(BuiltChild::Leaf(LeafRecord {
                        name: name.into_boxed_str(),
                        kind: NodeKind::Symlink,
                        allocated_bytes: alloc,
                        apparent_bytes: apparent,
                        modified: modified_seconds(&meta),
                        hidden,
                        lossy_name,
                        hardlink: None,
                    }));
                } else {
                    ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
                }
                continue;
            }

            let Ok(meta) = fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                if ctx.never_scanned.contains(&path) {
                    continue;
                }
                if ctx.options.one_filesystem
                    && ctx
                        .foreign_mounts
                        .as_ref()
                        .is_some_and(|m| m.contains(&path))
                {
                    continue;
                }
                if ctx.options.one_filesystem
                    && ctx.foreign_mounts.is_none()
                    && device_of(&meta) != ctx.root_dev
                {
                    continue;
                }
                if let Some(id) = inode_of(&meta)
                    && !ctx
                        .visited_dirs
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(id)
                {
                    continue;
                }
                if ctx
                    .options
                    .max_depth
                    .is_some_and(|limit| dir.depth + 1 > limit)
                {
                    continue;
                }
                subdirs.push(Arc::new(PendingDir::new(
                    name,
                    path,
                    Some(Arc::clone(dir)),
                    dir.depth + 1,
                    modified_seconds(&meta),
                    hidden,
                    lossy_name,
                )));
                continue;
            }

            let (alloc, apparent) = measure_both(&meta);
            local_files += u64::from(meta.is_file());
            local_alloc = local_alloc.saturating_add(alloc);
            local_apparent = local_apparent.saturating_add(apparent);
            local_children.push(BuiltChild::Leaf(LeafRecord {
                name: name.into_boxed_str(),
                kind: if meta.is_file() {
                    NodeKind::File
                } else {
                    NodeKind::Other
                },
                allocated_bytes: alloc,
                apparent_bytes: apparent,
                modified: modified_seconds(&meta),
                hidden,
                lossy_name,
                hardlink: inode_of(&meta).map(Box::new),
            }));
            continue;
        }

        if kind == NodeKind::Directory {
            if ctx.never_scanned.contains(&path) {
                continue;
            }
            if ctx.options.one_filesystem
                && ctx
                    .foreign_mounts
                    .as_ref()
                    .is_some_and(|m| m.contains(&path))
            {
                continue;
            }

            // Consult the cache-skip oracle BEFORE reading directory metadata
            // or opening the directory.
            if let Some(oracle) = &ctx.skip_oracle
                && let Some(reused) = oracle.try_reuse(&path)
            {
                let reused_root = reused.node(reused.root());
                ctx.progress
                    .skipped_fresh_dirs
                    .fetch_add(u64::from(reused_root.dirs), Ordering::Relaxed);
                ctx.progress
                    .dirs
                    .fetch_add(u64::from(reused_root.dirs), Ordering::Relaxed);
                ctx.progress
                    .files
                    .fetch_add(reused_root.files, Ordering::Relaxed);
                ctx.progress
                    .allocated_bytes
                    .fetch_add(reused_root.allocated_bytes, Ordering::Relaxed);
                ctx.progress
                    .apparent_bytes
                    .fetch_add(reused_root.apparent_bytes, Ordering::Relaxed);
                if let Some(hook) = &ctx.on_dir_tally {
                    hook(
                        &path,
                        reused_root.allocated_bytes,
                        reused_root.apparent_bytes,
                        reused_root.files,
                    );
                }
                local_children.push(BuiltChild::Grafted(Box::new(
                    GraftedChild {
                        name,
                        arena: Arc::new(reused),
                        hidden,
                        lossy_name,
                    },
                )));
                continue;
            }

            let Ok(facts) = entry.dir_facts() else {
                ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            };
            if facts.evicted {
                continue;
            }
            if ctx.options.one_filesystem
                && ctx.foreign_mounts.is_none()
                && facts.device != ctx.root_dev
            {
                continue;
            }
            if ctx.options.follow_links
                && let Some(id) = facts.inode
                && !ctx
                    .visited_dirs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(id)
            {
                continue;
            }
            if ctx
                .options
                .max_depth
                .is_some_and(|limit| dir.depth + 1 > limit)
            {
                continue;
            }
            subdirs.push(Arc::new(PendingDir::new(
                name,
                path,
                Some(Arc::clone(dir)),
                dir.depth + 1,
                facts.modified,
                hidden,
                lossy_name,
            )));
            continue;
        }

        // Leaf file or special node.
        let Ok(facts) = entry.leaf_facts(ctx.options.follow_links) else {
            ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let (alloc, apparent) = if kind == NodeKind::Other {
            (0, 0)
        } else {
            (facts.allocated_bytes, facts.apparent_bytes)
        };
        if kind == NodeKind::File {
            local_files += 1;
        }
        local_alloc = local_alloc.saturating_add(alloc);
        local_apparent = local_apparent.saturating_add(apparent);
        local_children.push(BuiltChild::Leaf(LeafRecord {
            name: name.into_boxed_str(),
            kind,
            allocated_bytes: alloc,
            apparent_bytes: apparent,
            modified: facts.modified,
            hidden,
            lossy_name,
            hardlink: (kind == NodeKind::File)
                .then_some(facts.hardlink)
                .flatten()
                .map(Box::new),
        }));
    }

    if local_files > 0 || local_alloc > 0 || local_apparent > 0 {
        ctx.progress.files.fetch_add(local_files, Ordering::Relaxed);
        ctx.progress
            .allocated_bytes
            .fetch_add(local_alloc, Ordering::Relaxed);
        ctx.progress
            .apparent_bytes
            .fetch_add(local_apparent, Ordering::Relaxed);
        if let Some(hook) = &ctx.on_dir_tally {
            hook(&dir.path, local_alloc, local_apparent, local_files);
        }
    }

    if !local_children.is_empty() {
        dir.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(local_children);
    }

    // Add every subdirectory to `pending` BEFORE spawning or deferring so the
    // `+1` sentinel guarantees `dir` cannot complete prematurely.
    if !subdirs.is_empty() {
        dir.pending.fetch_add(subdirs.len(), Ordering::AcqRel);
        let mut stale_deferred: Vec<(u64, Arc<PendingDir>)> = Vec::new();
        for sub in subdirs {
            if let Some(oracle) = &ctx.skip_oracle
                && let DirFreshness::Stale { scanned_at } =
                    oracle.freshness(&sub.path)
            {
                stale_deferred.push((scanned_at, sub));
                continue;
            }
            let ctx = Arc::clone(ctx);
            scope.spawn(move |s| {
                scan_dir(s, &ctx, &sub);
            });
        }
        if !stale_deferred.is_empty() {
            ctx.deferred_stale
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(stale_deferred);
        }
    }

    // Release this directory's own `+1` sentinel.
    complete_one(ctx, dir);
}

fn complete_one(ctx: &WalkContext, start: &Arc<PendingDir>) {
    let mut current = Arc::clone(start);
    loop {
        if current.pending.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        let incomplete = current.incomplete.load(Ordering::Relaxed);
        let leaves = std::mem::take(
            &mut *current
                .leaves
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let children = std::mem::take(
            &mut *current
                .children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let built = BuiltNode {
            name: current.name.clone(),
            modified: current.modified,
            read_error: current.read_error.load(Ordering::Relaxed),
            hidden: current.hidden,
            lossy_name: current.lossy_name,
            leaves,
            children,
        };
        if let Some(parent) = current.parent.as_ref().map(Arc::clone) {
            if incomplete {
                parent.incomplete.store(true, Ordering::Relaxed);
            }
            let child = if !incomplete
                && !ctx.cancel.load(Ordering::Relaxed)
                && let Some(hook) = &ctx.on_dir_complete
                && should_checkpoint_dir(current.depth, &built)
            {
                let scanned_at = unix_now();
                let mut arena =
                    materialize_arena(current.path.clone(), built, scanned_at);
                arena.aggregate(
                    ctx.options.dedup_hardlinks,
                    ctx.options.include_hidden,
                );
                let arena = Arc::new(arena);
                hook(current.path.clone(), current.depth, Arc::clone(&arena));
                BuiltChild::Grafted(Box::new(GraftedChild {
                    name: current.name.clone(),
                    arena,
                    hidden: current.hidden,
                    lossy_name: current.lossy_name,
                }))
            } else {
                BuiltChild::Dir(Box::new(built))
            };
            parent
                .children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(child);
            current = parent;
        } else {
            *ctx.finished_root
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(built);
            return;
        }
    }
}

/// Minimum node count before a `depth == 2` directory is checkpointed
/// independently (every `depth == 1` directory is always eligible).
const DEPTH2_CHECKPOINT_THRESHOLD: usize = 512;

fn should_checkpoint_dir(depth: usize, built: &BuiltNode) -> bool {
    match depth {
        1 => true,
        2 => subtree_has_at_least(built, DEPTH2_CHECKPOINT_THRESHOLD),
        _ => false,
    }
}

fn subtree_has_at_least(root: &BuiltNode, threshold: usize) -> bool {
    let mut count = 0usize;
    let mut stack: Vec<&BuiltNode> = vec![root];
    while let Some(node) = stack.pop() {
        count = count.saturating_add(node.leaves.len());
        if count >= threshold {
            return true;
        }
        for child in &node.children {
            match child {
                BuiltChild::Leaf(_) => {
                    count += 1;
                }
                BuiltChild::Dir(dir) => {
                    count += 1;
                    stack.push(dir);
                }
                BuiltChild::Grafted(grafted) => {
                    count = count.saturating_add(grafted.arena.len());
                }
            }
            if count >= threshold {
                return true;
            }
        }
    }
    false
}

fn append_leaves_to_arena(
    arena: &mut TreeArena,
    parent_id: NodeId,
    leaves: &LeafSlab,
) {
    let count = leaves.len();
    if count == 0 {
        return;
    }
    let first_child_node_idx = arena.len();
    let mut start = 0usize;
    for i in 0..count {
        let end = leaves.name_ends[i] as usize;
        let name =
            std::str::from_utf8(&leaves.names[start..end]).unwrap_or_default();
        start = end;
        let (kind, hidden, lossy) = LeafSlab::unpack_meta(leaves.meta[i]);
        let mut flags = NodeFlags::new(kind);
        flags.set(NodeFlags::HIDDEN, hidden);
        flags.set(NodeFlags::LOSSY_NAME, lossy);
        arena.add_child_raw(
            parent_id,
            name,
            kind,
            leaves.allocated[i],
            leaves.apparent[i],
            leaves.modified[i],
            flags,
        );
    }
    for &(leaf_idx, dev, ino) in &leaves.hardlinks {
        if let Some(id) =
            NodeId::from_index(first_child_node_idx + leaf_idx as usize)
        {
            arena.record_hardlink(id, dev, ino);
        }
    }
}

fn estimate_built_capacity(root: &BuiltNode) -> (usize, usize) {
    let mut nodes = 1usize;
    let mut bytes = root.name.len();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        nodes += dir.leaves.len();
        bytes += dir.leaves.names.len();
        for child in &dir.children {
            match child {
                BuiltChild::Leaf(leaf) => {
                    nodes += 1;
                    bytes += leaf.name.len();
                }
                BuiltChild::Dir(sub) => {
                    nodes += 1;
                    bytes += sub.name.len();
                    stack.push(sub);
                }
                BuiltChild::Grafted(g) => {
                    nodes += g.arena.len();
                    bytes += g.arena.strings().byte_len() + g.name.len();
                }
            }
        }
    }
    (nodes, bytes)
}

/// Convert the completed parallel tree into a flat [`TreeArena`] in one
/// iterative pass with exact pre-sized backing arrays and zero hash-table build.
fn materialize_arena(
    root_path: PathBuf,
    root: BuiltNode,
    scanned_at: u64,
) -> TreeArena {
    let (expected_nodes, expected_bytes) = estimate_built_capacity(&root);
    let mut arena = TreeArena::with_exact_capacity(
        root_path,
        &root.name,
        expected_nodes,
        expected_bytes,
    );
    let root_id = arena.root();
    {
        let r = arena.node_mut(root_id);
        r.modified = root.modified;
        r.flags.set(NodeFlags::READ_ERROR, root.read_error);
        r.flags.set(NodeFlags::HIDDEN, root.hidden);
        r.flags.set(NodeFlags::LOSSY_NAME, root.lossy_name);
        r.set_status(SubtreeStatus::Fresh { scanned_at });
    }
    append_leaves_to_arena(&mut arena, root_id, &root.leaves);

    let mut stack: Vec<(NodeId, Vec<BuiltChild>)> =
        vec![(root_id, root.children)];

    while let Some((parent_id, children)) = stack.pop() {
        for child in children {
            match child {
                BuiltChild::Leaf(leaf) => {
                    let mut flags = NodeFlags::new(leaf.kind);
                    flags.set(NodeFlags::HIDDEN, leaf.hidden);
                    flags.set(NodeFlags::LOSSY_NAME, leaf.lossy_name);
                    let id = arena.add_child(
                        parent_id,
                        &leaf.name,
                        leaf.kind,
                        leaf.allocated_bytes,
                        leaf.apparent_bytes,
                        leaf.modified,
                        flags,
                    );
                    if let Some(link) = leaf.hardlink {
                        let (dev, ino) = *link;
                        arena.record_hardlink(id, dev, ino);
                    }
                }
                BuiltChild::Dir(dir) => {
                    let dir = *dir;
                    let mut flags = NodeFlags::new(NodeKind::Directory);
                    flags.set(NodeFlags::READ_ERROR, dir.read_error);
                    flags.set(NodeFlags::HIDDEN, dir.hidden);
                    flags.set(NodeFlags::LOSSY_NAME, dir.lossy_name);
                    let id = arena.add_child_raw(
                        parent_id,
                        &dir.name,
                        NodeKind::Directory,
                        0,
                        0,
                        dir.modified,
                        flags,
                    );
                    arena
                        .node_mut(id)
                        .set_status(SubtreeStatus::Fresh { scanned_at });
                    append_leaves_to_arena(&mut arena, id, &dir.leaves);
                    if !dir.children.is_empty() {
                        stack.push((id, dir.children));
                    }
                }
                BuiltChild::Grafted(grafted) => {
                    let GraftedChild {
                        name,
                        arena: sub_arena,
                        hidden,
                        lossy_name,
                    } = *grafted;
                    let grafted_id =
                        arena.graft_subtree(parent_id, &sub_arena, Some(&name));
                    let node = arena.node_mut(grafted_id);
                    node.flags.set(NodeFlags::HIDDEN, hidden);
                    node.flags.set(NodeFlags::LOSSY_NAME, lossy_name);
                }
            }
        }
    }

    arena.finish_building();
    arena
}

/// Simple map-backed [`SkipOracle`] combining fresh DB shards, stale DB
/// timestamps (for defer-last ordering), and recently completed immediate
/// priority revalidations.
pub struct MapSkipOracle {
    /// Already-materialized subtrees (e.g. finished `Priority::Immediate`
    /// scans that the resumed background scan must not re-walk).
    pub in_memory: Arc<Mutex<FxHashMap<PathBuf, TreeArena>>>,
    /// Fresh DB shard `dir_id`s preloaded before the walk began (skipped).
    pub db_fresh: FxHashMap<PathBuf, u64>,
    /// Stale DB shard `scanned_at` timestamps preloaded before the walk began
    /// (deferred behind unscanned directories, refreshed oldest-first).
    pub db_stale: FxHashMap<PathBuf, u64>,
    /// Loader callback for `db_fresh` hits.
    pub loader: Option<ShardLoader>,
}

impl std::fmt::Debug for MapSkipOracle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MapSkipOracle")
            .field("db_fresh_len", &self.db_fresh.len())
            .field("db_stale_len", &self.db_stale.len())
            .field("has_loader", &self.loader.is_some())
            .finish_non_exhaustive()
    }
}

impl SkipOracle for MapSkipOracle {
    fn has_fresh(&self) -> bool {
        !self.db_fresh.is_empty()
            || !self
                .in_memory
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
    }

    fn try_reuse(&self, path: &Path) -> Option<TreeArena> {
        if let Some(arena) = self
            .in_memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(path)
        {
            return Some(arena.clone());
        }
        if let Some(&dir_id) = self.db_fresh.get(path)
            && let Some(loader) = &self.loader
        {
            return loader(dir_id, path);
        }
        None
    }

    fn freshness(&self, path: &Path) -> DirFreshness {
        if self
            .in_memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(path)
            || self.db_fresh.contains_key(path)
        {
            return DirFreshness::Fresh;
        }
        if let Some(&scanned_at) = self.db_stale.get(path) {
            return DirFreshness::Stale { scanned_at };
        }
        DirFreshness::Unscanned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestOracle {
        fresh: FxHashMap<PathBuf, TreeArena>,
        stale: FxHashMap<PathBuf, u64>,
    }

    impl SkipOracle for TestOracle {
        fn try_reuse(&self, path: &Path) -> Option<TreeArena> {
            self.fresh.get(path).cloned()
        }

        fn freshness(&self, path: &Path) -> DirFreshness {
            if self.fresh.contains_key(path) {
                DirFreshness::Fresh
            } else if let Some(&scanned_at) = self.stale.get(path) {
                DirFreshness::Stale { scanned_at }
            } else {
                DirFreshness::Unscanned
            }
        }
    }

    #[test]
    fn default_worker_heuristic_scales_and_caps_quietly() {
        assert_eq!(default_worker_threads(0), 1);
        assert_eq!(default_worker_threads(1), 1);
        assert_eq!(default_worker_threads(4), 4);
        assert_eq!(default_worker_threads(8), 4);
        assert_eq!(default_worker_threads(12), 6);
        assert_eq!(default_worker_threads(18), 9);
        assert_eq!(default_worker_threads(64), 32);
        assert_eq!(default_worker_threads(128), 32);
    }

    #[test]
    fn gate_machine_nested_holds_and_terminal_cancel() {
        use gate_machine::{Effect, Event, State, step};

        let (state, effects) = step(State::Running, Event::Pause);
        assert!(matches!(state, State::Paused { holds } if holds.get() == 1));
        assert!(effects.is_empty());

        let (state, effects) = step(state, Event::Pause);
        assert!(matches!(state, State::Paused { holds } if holds.get() == 2));
        assert!(effects.is_empty());

        let (state, effects) = step(state, Event::Resume);
        assert!(matches!(state, State::Paused { holds } if holds.get() == 1));
        assert!(effects.is_empty());

        let (state, effects) = step(state, Event::Resume);
        assert_eq!(state, State::Running);
        assert_eq!(effects, vec![Effect::WakeAll]);

        // Duplicate resume in Running is a safe no-op.
        let (state, effects) = step(state, Event::Resume);
        assert_eq!(state, State::Running);
        assert!(effects.is_empty());

        // Cancel wakes parked workers and is terminal.
        let (state, _) = step(state, Event::Pause);
        let (state, effects) = step(state, Event::Cancel);
        assert_eq!(state, State::Cancelled);
        assert_eq!(effects, vec![Effect::WakeAll]);

        let (state, effects) = step(state, Event::Pause);
        assert_eq!(state, State::Cancelled);
        assert!(effects.is_empty());
    }

    #[test]
    fn walk_machine_orders_unscanned_then_oldest_stale_and_stops_on_cancel() {
        use walk_machine::{Effect, Event, State, step};

        let (state, effects) = step(State::Idle, Event::Start);
        assert_eq!(state, State::WalkingUnscanned);
        assert!(matches!(effects.as_slice(), [Effect::WalkUnscannedRoot]));

        let newer = Arc::new(PendingDir::new(
            "newer".into(),
            PathBuf::from("/newer"),
            None,
            1,
            0,
            false,
            false,
        ));
        let older = Arc::new(PendingDir::new(
            "older".into(),
            PathBuf::from("/older"),
            None,
            1,
            0,
            false,
            false,
        ));
        let (state, effects) = step(
            state,
            Event::PhaseCompleted {
                deferred_stale: vec![(2000, newer), (1000, older)],
                cancelled: false,
            },
        );
        assert_eq!(state, State::WalkingStale);
        let [Effect::WalkStaleBatch(batch)] = effects.as_slice() else {
            panic!("expected WalkStaleBatch");
        };
        let names: Vec<&str> = batch.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["older", "newer"]);

        // Cancellation terminates immediately even if more stale dirs remain.
        let extra = Arc::new(PendingDir::new(
            "extra".into(),
            PathBuf::from("/extra"),
            None,
            1,
            0,
            false,
            false,
        ));
        let (state, effects) = step(
            state,
            Event::PhaseCompleted {
                deferred_stale: vec![(500, extra)],
                cancelled: true,
            },
        );
        assert_eq!(state, State::Done);
        assert!(effects.is_empty());
    }

    #[test]
    fn unscanned_dirs_walk_before_stale_and_oldest_stale_walks_first() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().canonicalize().expect("canonical");
        let fresh_dir = root.join("a_fresh");
        let stale_newer = root.join("b_stale_newer");
        let stale_older = root.join("c_stale_older");
        let unscanned = root.join("d_unscanned");
        for dir in [&fresh_dir, &stale_newer, &stale_older, &unscanned] {
            fs::create_dir(dir).expect("mkdir");
            fs::write(dir.join("file.bin"), b"1234").expect("write");
        }

        let mut fresh_arena = TreeArena::new(fresh_dir.clone(), "a_fresh");
        let fresh_root = fresh_arena.root();
        fresh_arena.add_child(
            fresh_root,
            "file.bin",
            NodeKind::File,
            4096,
            4,
            0,
            NodeFlags::new(NodeKind::File),
        );
        fresh_arena.aggregate(true, true);

        let mut fresh = FxHashMap::default();
        fresh.insert(fresh_dir.clone(), fresh_arena);
        let mut stale = FxHashMap::default();
        stale.insert(stale_newer.clone(), 2000);
        stale.insert(stale_older.clone(), 1000);

        let oracle: Arc<dyn SkipOracle> = Arc::new(TestOracle { fresh, stale });
        let visited: Arc<Mutex<Vec<PathBuf>>> =
            Arc::new(Mutex::new(Vec::new()));
        let visited_hook = Arc::clone(&visited);
        let on_dir_start: DirStartHook = Arc::new(move |dir: &Path| {
            visited_hook
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(dir.to_path_buf());
        });

        // Single worker guarantees deterministic FIFO ordering within each tier.
        let options = ScanOptions {
            apparent_size: true,
            threads: Some(1),
            ..ScanOptions::default()
        };
        let outcome = walk_parallel(
            &root,
            WalkConfig {
                options: &options,
                cancel: Arc::new(AtomicBool::new(false)),
                progress: Arc::new(WalkProgress::default()),
                pause_gate: None,
                skip_oracle: Some(oracle),
                on_dir_start: Some(on_dir_start),
                on_dir_tally: None,
                on_dir_complete: None,
            },
        )
        .expect("walk");

        let order = visited
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert!(!order.contains(&fresh_dir), "fresh dir must be skipped");
        let pos_unscanned = order
            .iter()
            .position(|p| p == &unscanned)
            .expect("unscanned walked");
        let pos_older = order
            .iter()
            .position(|p| p == &stale_older)
            .expect("older stale walked");
        let pos_newer = order
            .iter()
            .position(|p| p == &stale_newer)
            .expect("newer stale walked");
        assert!(
            pos_unscanned < pos_older && pos_older < pos_newer,
            "expected unscanned < older_stale < newer_stale, got {order:?}"
        );
        assert_eq!(outcome.stats.apparent_bytes, 16);
        assert_eq!(outcome.stats.files, 4);
    }

    #[test]
    fn built_child_stays_compact() {
        assert!(
            std::mem::size_of::<BuiltChild>() <= 64,
            "BuiltChild is paid per scanned file and grew to {} bytes",
            std::mem::size_of::<BuiltChild>()
        );
    }
}

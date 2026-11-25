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
use crate::stream::{
    device_of, inode_of, measure_both, modified_seconds, nlink_of,
};
use crate::tree::{NodeFlags, NodeId, TreeArena};
use crate::types::{NodeKind, ScanOptions, ScanStats, SubtreeStatus, unix_now};

/// Callback invoked right as a directory begins walking.
pub type DirStartHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// Callback invoked once per walked directory with its direct file totals:
/// `(dir_path, allocated_bytes, apparent_bytes, files)`.
pub type DirTallyHook = Arc<dyn Fn(&Path, u64, u64, u64) + Send + Sync>;

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
/// Metadata walks scale strongly up to ~6–8 workers; past 8, VFS lock
/// contention adds CPU heat and fan noise without meaningful wall-time gains.
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
        if half > 8 { 8 } else { half }
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
        root_dev,
        foreign_mounts,
        never_scanned: never,
        visited_dirs: Mutex::new(visited_dirs),
        cancel: config.cancel,
        progress: config.progress,
        pause_gate: config.pause_gate,
        skip_oracle: config.skip_oracle,
        on_dir_start: config.on_dir_start,
        on_dir_tally: config.on_dir_tally,
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
                .push(BuiltChild::Grafted {
                    name: sub.name.clone(),
                    arena: reused,
                    hidden: sub.hidden,
                    lossy_name: sub.lossy_name,
                });
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
    root_dev: u64,
    foreign_mounts: Option<FxHashSet<PathBuf>>,
    never_scanned: FxHashSet<PathBuf>,
    visited_dirs: Mutex<FxHashSet<(u64, u64)>>,
    cancel: Arc<AtomicBool>,
    progress: Arc<WalkProgress>,
    pause_gate: Option<Arc<PauseGate>>,
    skip_oracle: Option<Arc<dyn SkipOracle>>,
    on_dir_start: Option<DirStartHook>,
    on_dir_tally: Option<DirTallyHook>,
    #[cfg(windows)]
    volume: Option<u32>,
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
    /// Starts at `1` (the `+1` sentinel for this directory's own scan).
    pending: AtomicUsize,
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
            pending: AtomicUsize::new(1),
            children: Mutex::new(Vec::new()),
        }
    }
}

pub(crate) enum BuiltChild {
    Leaf(LeafRecord),
    Dir(BuiltNode),
    Grafted {
        name: String,
        arena: TreeArena,
        hidden: bool,
        lossy_name: bool,
    },
}

pub(crate) struct LeafRecord {
    pub name: String,
    pub kind: NodeKind,
    pub allocated_bytes: u64,
    pub apparent_bytes: u64,
    pub modified: i64,
    pub hidden: bool,
    pub lossy_name: bool,
    pub hardlink: Option<(u64, u64)>,
}

pub(crate) struct BuiltNode {
    pub name: String,
    pub modified: i64,
    pub read_error: bool,
    pub hidden: bool,
    pub lossy_name: bool,
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
            children: Vec::new(),
        }
    }
}

struct DirFacts {
    device: u64,
    modified: i64,
    evicted: bool,
    inode: Option<(u64, u64)>,
}

struct LeafFacts {
    allocated_bytes: u64,
    apparent_bytes: u64,
    modified: i64,
    hardlink: Option<(u64, u64)>,
}

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

#[cfg(not(any(target_os = "macos", windows)))]
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

#[cfg(not(any(target_os = "macos", windows)))]
fn read_dir_entries(
    path: &Path,
    _ctx: &WalkContext,
) -> io::Result<fs::ReadDir> {
    fs::read_dir(path)
}

fn scan_dir(scope: &Scope<'_>, ctx: &Arc<WalkContext>, dir: &Arc<PendingDir>) {
    if let Some(gate) = &ctx.pause_gate {
        gate.wait_if_paused(&ctx.cancel);
    }

    if ctx.cancel.load(Ordering::Relaxed) {
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

        let Ok(kind) = entry.kind() else {
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
                        name,
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
                name,
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
                hardlink: inode_of(&meta),
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
                local_children.push(BuiltChild::Grafted {
                    name,
                    arena: reused,
                    hidden,
                    lossy_name,
                });
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
            name,
            kind,
            allocated_bytes: alloc,
            apparent_bytes: apparent,
            modified: facts.modified,
            hidden,
            lossy_name,
            hardlink: (kind == NodeKind::File)
                .then_some(facts.hardlink)
                .flatten(),
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
            children,
        };
        if let Some(parent) = current.parent.as_ref().map(Arc::clone) {
            parent
                .children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(BuiltChild::Dir(built));
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

/// Convert the completed parallel tree into a flat [`TreeArena`] in one
/// iterative pass.
fn materialize_arena(
    root_path: PathBuf,
    root: BuiltNode,
    scanned_at: u64,
) -> TreeArena {
    let mut arena = TreeArena::new(root_path, &root.name);
    {
        let r = arena.node_mut(arena.root());
        r.modified = root.modified;
        r.flags.set(NodeFlags::READ_ERROR, root.read_error);
        r.flags.set(NodeFlags::HIDDEN, root.hidden);
        r.flags.set(NodeFlags::LOSSY_NAME, root.lossy_name);
        r.set_status(SubtreeStatus::Fresh { scanned_at });
    }

    let mut stack: Vec<(NodeId, Vec<BuiltChild>)> =
        vec![(arena.root(), root.children)];

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
                    if let Some((dev, ino)) = leaf.hardlink {
                        arena.record_hardlink(id, dev, ino);
                    }
                }
                BuiltChild::Dir(dir) => {
                    let mut flags = NodeFlags::new(NodeKind::Directory);
                    flags.set(NodeFlags::READ_ERROR, dir.read_error);
                    flags.set(NodeFlags::HIDDEN, dir.hidden);
                    flags.set(NodeFlags::LOSSY_NAME, dir.lossy_name);
                    let id = arena.add_child(
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
                    if !dir.children.is_empty() {
                        stack.push((id, dir.children));
                    }
                }
                BuiltChild::Grafted {
                    name,
                    arena: sub_arena,
                    hidden,
                    lossy_name,
                } => {
                    let grafted_id =
                        arena.graft_subtree(parent_id, &sub_arena, Some(&name));
                    let node = arena.node_mut(grafted_id);
                    node.flags.set(NodeFlags::HIDDEN, hidden);
                    node.flags.set(NodeFlags::LOSSY_NAME, lossy_name);
                }
            }
        }
    }

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
        assert_eq!(default_worker_threads(18), 8);
        assert_eq!(default_worker_threads(64), 8);
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
}

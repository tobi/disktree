//! Filesystem scanning backed by `disktree-scanner`.
//!
//! `disktree-scanner` owns parallel traversal, platform fast paths
//! (`getattrlistbulk` on macOS, `GetFileInformationByHandleEx` + NTFS MFT on
//! Windows), compact arena storage, Protobuf caching, and stale-while-revalidate
//! (SWR) sessions. This module projects `TreeArena` snapshots into classified
//! [`Node`] trees for the treemap and exposes [`ScanHandle`] for UI polling.

use std::io;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use disktree_scanner::{
    CachePolicy, DirTallyHook, Freshness, HostKey, NodeFlags, NodeId,
    NodeKind as ScannerKind, ScanEvent, ScanSession, Scanner, SkipOracle,
    TreeArena, WalkConfig, WalkProgress, walk::walk_parallel,
};
use rustc_hash::FxHashMap;

use crate::tree::{Metric, Node, NodeKind, Seen, aggregate, aggregate_deduped};

/// How a scan measures and filters the tree.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct ScanOptions {
    /// Measure apparent length instead of allocated blocks. Both values are
    /// stored in every `TreeArena`, so toggling this in the UI needs no rescan.
    pub apparent_size: bool,
    /// Follow symlinks. Off by default.
    pub follow_links: bool,
    /// Include dotfiles and dot-directories. Walks record hidden entries with
    /// a flag so toggling this needs no rescan.
    pub include_hidden: bool,
    /// Stay on the root's volume: skip other disks, pseudo filesystems,
    /// network shares and overlapping mount views. Local Btrfs scans include
    /// distinct same-filesystem subvolumes; remote scans stop at every mount.
    pub one_filesystem: bool,
    /// Stop descending past this depth.
    pub max_depth: Option<usize>,
    /// Count a hardlinked file once instead of once per link.
    pub dedup_hardlinks: bool,
    /// Whether children are ranked by bytes or by file count.
    pub metric: Metric,
    /// Optional fixed worker count; `None` uses available parallelism.
    #[serde(default)]
    pub threads: Option<usize>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            apparent_size: false,
            follow_links: false,
            include_hidden: true,
            one_filesystem: true,
            max_depth: None,
            dedup_hardlinks: true,
            metric: Metric::Bytes,
            threads: None,
        }
    }
}

impl ScanOptions {
    /// Convert to the underlying `disktree-scanner` traversal options.
    #[must_use]
    pub const fn to_scanner(&self) -> disktree_scanner::ScanOptions {
        disktree_scanner::ScanOptions {
            apparent_size: self.apparent_size,
            follow_links: self.follow_links,
            include_hidden: self.include_hidden,
            one_filesystem: self.one_filesystem,
            max_depth: self.max_depth,
            dedup_hardlinks: self.dedup_hardlinks,
            threads: self.threads,
            backend: disktree_scanner::ScanBackend::Auto,
        }
    }
}

/// A point-in-time view of scan progress for the UI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanSnapshot {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: u64,
    pub finished: bool,
    pub cancelled: bool,
    /// Up to 50 unreadable paths, most recent last.
    pub messages: Vec<String>,
}

/// Shared progress + live treemap preview handle for an in-flight scan.
#[derive(Debug)]
pub struct ScanProgress {
    walk: Arc<WalkProgress>,
    cancel: Arc<AtomicBool>,
    finished: AtomicBool,
    apparent_size: bool,
    preview_root: OnceLock<PathBuf>,
    preview: Arc<Mutex<crate::scan_preview::Builder>>,
}

impl Default for ScanProgress {
    fn default() -> Self {
        Self::new(
            Arc::new(WalkProgress::default()),
            Arc::new(AtomicBool::new(false)),
            false,
            None,
        )
    }
}

impl ScanProgress {
    fn new(
        walk: Arc<WalkProgress>,
        cancel: Arc<AtomicBool>,
        apparent_size: bool,
        root: Option<PathBuf>,
    ) -> Self {
        Self::with_preview(
            walk,
            cancel,
            apparent_size,
            root,
            Arc::new(Mutex::new(crate::scan_preview::Builder::default())),
        )
    }

    fn with_preview(
        walk: Arc<WalkProgress>,
        cancel: Arc<AtomicBool>,
        apparent_size: bool,
        root: Option<PathBuf>,
        preview: Arc<Mutex<crate::scan_preview::Builder>>,
    ) -> Self {
        let preview_root = OnceLock::new();
        if let Some(root) = root {
            let _ = preview_root.set(root);
        }
        Self {
            walk,
            cancel,
            finished: AtomicBool::new(false),
            apparent_size,
            preview_root,
            preview,
        }
    }

    pub fn preview(&self) -> crate::scan_preview::Preview {
        lock(&self.preview).snapshot()
    }

    fn preview_add(&self, path: &Path, bytes: u64, files: u64) {
        let Some(root) = self.preview_root.get() else {
            return;
        };
        let Ok(below) = path.strip_prefix(root) else {
            return;
        };
        let name = below
            .components()
            .next()
            .map(|part| part.as_os_str().to_string_lossy());
        let mut preview = lock(&self.preview);
        preview.add(name.as_deref(), bytes, files);
        preview.directory(path);
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    fn finish(&self) {
        self.finished.store(true, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ScanSnapshot {
        let bytes = if self.apparent_size {
            self.walk.apparent_bytes.load(Ordering::Relaxed)
        } else {
            self.walk.allocated_bytes.load(Ordering::Relaxed)
        };
        ScanSnapshot {
            files: self.walk.files.load(Ordering::Relaxed),
            dirs: self.walk.dirs.load(Ordering::Relaxed),
            bytes,
            errors: self.walk.errors.load(Ordering::Relaxed),
            finished: self.finished.load(Ordering::Relaxed),
            cancelled: self.is_cancelled(),
            messages: self.walk.messages(),
        }
    }
}

/// Incremental outcome emitted by [`ScanHandle::poll_update`].
#[derive(Debug)]
pub enum ScanUpdate {
    /// Immediate snapshot served from the cache DB (`Fresh`, `Stale`, or
    /// `Partial`). If `freshness` is `Fresh`, no further walk will run.
    Cached {
        arena: Arc<TreeArena>,
        node: Node,
        freshness: Freshness,
    },
    /// A directory has started actively walking during a refresh.
    SubtreeUpdating { path: PathBuf },
    /// A prioritized subtree finished scanning while background revalidation
    /// continues on the rest of the root.
    SubtreeRefreshed {
        path: PathBuf,
        arena: Arc<TreeArena>,
    },
    /// Completed live scan (all subtrees up to date and persisted to cache).
    Live { arena: Arc<TreeArena>, node: Node },
    /// Non-fatal or fatal scan error.
    Error(io::Error),
    /// Background revalidation finished without a new snapshot (e.g. fresh
    /// cache hit already emitted or cancelled).
    Finished,
}

/// A subtree already measured, reused by a wider scan instead of walked
/// again: widening from `~` to `/` only reads what is outside `~`.
#[derive(Clone, Debug)]
pub struct Known {
    /// Canonical path of the already-measured subtree.
    pub path: PathBuf,
    pub arena: Arc<TreeArena>,
}

struct KnownSkipOracle {
    path: PathBuf,
    arena: Arc<TreeArena>,
}

impl SkipOracle for KnownSkipOracle {
    fn try_reuse(&self, path: &Path) -> Option<TreeArena> {
        (path == self.path).then(|| (*self.arena).clone())
    }
}

enum ScanBackend {
    Session(ScanSession),
    OneShot(std::sync::mpsc::Receiver<io::Result<TreeArena>>),
}

/// A scan running on background worker threads, backed by `disktree-scanner`.
pub struct ScanHandle {
    pub progress: Arc<ScanProgress>,
    options: ScanOptions,
    backend: ScanBackend,
}

impl std::fmt::Debug for ScanHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanHandle")
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl ScanHandle {
    /// Start an SWR scan backed by `scanner`'s persistent/auto cache DB.
    pub fn spawn_session(
        scanner: &Scanner,
        root: PathBuf,
        options: ScanOptions,
        policy: CachePolicy,
    ) -> io::Result<Self> {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
        let preview =
            Arc::new(Mutex::new(crate::scan_preview::Builder::default()));
        let preview_hook = Arc::clone(&preview);
        let hook_root = canonical.clone();
        let apparent = options.apparent_size;
        let on_dir_tally: DirTallyHook =
            Arc::new(move |dir, alloc, app, files| {
                let Ok(below) = dir.strip_prefix(&hook_root) else {
                    return;
                };
                let name = below
                    .components()
                    .next()
                    .map(|part| part.as_os_str().to_string_lossy());
                let bytes = if apparent { app } else { alloc };
                let mut builder = lock(&preview_hook);
                builder.add(name.as_deref(), bytes, files);
                builder.directory(dir);
            });
        let session = scanner.open_with_tally(
            HostKey::Local,
            root,
            options.to_scanner(),
            policy,
            Some(on_dir_tally),
        )?;
        let progress = Arc::new(ScanProgress::with_preview(
            session.progress(),
            Arc::new(AtomicBool::new(false)),
            options.apparent_size,
            Some(canonical),
            preview,
        ));
        Ok(Self {
            progress,
            options,
            backend: ScanBackend::Session(session),
        })
    }

    /// Start an in-memory scan without touching any persistent cache DB.
    pub fn spawn(root: PathBuf, options: ScanOptions) -> Self {
        Self::spawn_with(root, options, None)
    }

    /// Start an in-memory scan, reusing `known` where the walk reaches it.
    pub fn spawn_with(
        root: PathBuf,
        options: ScanOptions,
        known: Option<Known>,
    ) -> Self {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
        let walk_progress = Arc::new(WalkProgress::default());
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(ScanProgress::new(
            Arc::clone(&walk_progress),
            Arc::clone(&cancel),
            options.apparent_size,
            Some(canonical),
        ));
        let tally_progress = Arc::clone(&progress);
        let apparent = options.apparent_size;
        let on_dir_tally: DirTallyHook =
            Arc::new(move |dir, alloc, app, files| {
                let bytes = if apparent { app } else { alloc };
                tally_progress.preview_add(dir, bytes, files);
            });
        let (sender, receiver) = std::sync::mpsc::channel();
        let scanner_opts = options.to_scanner();
        let worker_progress = Arc::clone(&progress);
        let worker = thread::Builder::new().name("disktree-scan".into()).spawn(
            move || {
                let skip_oracle: Option<Arc<dyn SkipOracle>> =
                    known.map(|known| {
                        let oracle: Arc<dyn SkipOracle> =
                            Arc::new(KnownSkipOracle {
                                path: known.path,
                                arena: known.arena,
                            });
                        oracle
                    });
                let outcome = walk_parallel(
                    &root,
                    WalkConfig {
                        options: &scanner_opts,
                        cancel,
                        progress: walk_progress,
                        pause_gate: None,
                        skip_oracle,
                        on_dir_start: None,
                        on_dir_tally: Some(on_dir_tally),
                        on_dir_complete: None,
                    },
                )
                .map(|walk| walk.arena);
                worker_progress.finish();
                let _ = sender.send(outcome);
            },
        );
        if let Err(error) = worker {
            progress.walk.record_error(Path::new("scan worker"), &error);
            progress.finish();
        }
        Self {
            progress,
            options,
            backend: ScanBackend::OneShot(receiver),
        }
    }

    /// Ask the in-flight scan to stop.
    pub fn cancel(&self) {
        self.progress.cancel();
        if let ScanBackend::Session(session) = &self.backend {
            session.cancel();
        }
    }

    /// Preempt background SWR revalidation to refresh `path` first.
    pub fn prioritize(&self, path: &Path) {
        if let ScanBackend::Session(session) = &self.backend {
            let _ = session.prioritize(path);
        }
    }

    /// Most recently started directory path in the background walk, if any.
    pub fn active_path(&self) -> Option<PathBuf> {
        self.progress
            .walk
            .active_path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Drain the next incremental update from an SWR session or one-shot walk.
    pub fn poll_update(&self) -> Option<ScanUpdate> {
        match &self.backend {
            ScanBackend::Session(session) => loop {
                match session.try_recv() {
                    Ok(ScanEvent::Snapshot(snap)) => {
                        // Invariant #11: an incomplete ancestor stub (only a
                        // nested child was scanned earlier) must never replace
                        // the complete tree in the desktop UI.
                        if matches!(snap.freshness, Freshness::Partial { .. }) {
                            continue;
                        }
                        let node = arena_to_node(
                            &snap.arena,
                            self.options.apparent_size,
                            self.options.include_hidden,
                            self.options.dedup_hardlinks,
                            self.options.metric,
                        );
                        if matches!(snap.freshness, Freshness::Live { .. }) {
                            self.progress.finish();
                            return Some(ScanUpdate::Live {
                                arena: snap.arena,
                                node,
                            });
                        }
                        if matches!(snap.freshness, Freshness::Fresh { .. }) {
                            self.progress.finish();
                        }
                        return Some(ScanUpdate::Cached {
                            arena: snap.arena,
                            node,
                            freshness: snap.freshness,
                        });
                    }
                    Ok(ScanEvent::Error { message }) => {
                        self.progress.finish();
                        return Some(ScanUpdate::Error(io::Error::other(
                            message,
                        )));
                    }
                    Ok(ScanEvent::Finished)
                    | Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.progress.finish();
                        return Some(ScanUpdate::Finished);
                    }
                    Ok(ScanEvent::SubtreeUpdating { path, .. }) => {
                        return Some(ScanUpdate::SubtreeUpdating { path });
                    }
                    Ok(ScanEvent::SubtreeRefreshed { path, arena, .. }) => {
                        return Some(ScanUpdate::SubtreeRefreshed {
                            path,
                            arena,
                        });
                    }
                    Ok(
                        ScanEvent::Progress { .. }
                        | ScanEvent::StorageFallback { .. },
                    ) => {}
                    Err(std::sync::mpsc::TryRecvError::Empty) => return None,
                }
            },
            ScanBackend::OneShot(receiver) => match receiver.try_recv() {
                Ok(Ok(arena)) => {
                    let arena = Arc::new(arena);
                    let node = arena_to_node(
                        &arena,
                        self.options.apparent_size,
                        self.options.include_hidden,
                        self.options.dedup_hardlinks,
                        self.options.metric,
                    );
                    self.progress.finish();
                    Some(ScanUpdate::Live { arena, node })
                }
                Ok(Err(error)) => {
                    self.progress.finish();
                    Some(ScanUpdate::Error(error))
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.progress.finish();
                    Some(ScanUpdate::Error(io::Error::other(
                        "the scan worker stopped without a result",
                    )))
                }
            },
        }
    }

    /// Take the finished tree, if the walk is done (simple blocking/test API).
    pub fn poll(&self) -> Option<io::Result<Node>> {
        match self.poll_update()? {
            ScanUpdate::Live { node, .. }
            | ScanUpdate::Cached {
                node,
                freshness: Freshness::Fresh { .. },
                ..
            } => Some(Ok(node)),
            ScanUpdate::Cached { .. }
            | ScanUpdate::SubtreeUpdating { .. }
            | ScanUpdate::SubtreeRefreshed { .. } => None,
            ScanUpdate::Error(err) => Some(Err(err)),
            ScanUpdate::Finished => Some(Err(io::Error::other(
                "the scan finished without producing a tree",
            ))),
        }
    }
}

/// Project a compact [`TreeArena`] into a classified, aggregated [`Node`]
/// tree for the GPUI treemap and insights engine.
///
/// Because `TreeArena` stores both `allocated_bytes` and `apparent_bytes`,
/// hidden flags, and multi-link identities, toggling `apparent_size` or
/// `include_hidden` re-projects instantaneously without touching disk.
#[must_use]
pub fn arena_to_node(
    arena: &TreeArena,
    apparent_size: bool,
    include_hidden: bool,
    dedup_hardlinks: bool,
    metric: Metric,
) -> Node {
    let mut link_map: FxHashMap<NodeId, (u64, NonZeroU64)> =
        FxHashMap::default();
    if dedup_hardlinks && !arena.hardlinks().is_empty() {
        link_map.reserve(arena.hardlinks().len());
        for link in arena.hardlinks() {
            if let Some(ino) = NonZeroU64::new(link.ino) {
                link_map.insert(link.node, (link.dev, ino));
            }
        }
    }
    let mut root = build_node(
        arena,
        arena.root(),
        apparent_size,
        include_hidden,
        &link_map,
        true,
    );
    if dedup_hardlinks {
        aggregate_deduped(&mut root, metric, &Seen::new());
    } else {
        aggregate(&mut root, metric);
    }
    crate::classify::classify(&mut root);
    root
}

fn build_node(
    arena: &TreeArena,
    id: NodeId,
    apparent_size: bool,
    include_hidden: bool,
    link_map: &FxHashMap<NodeId, (u64, NonZeroU64)>,
    is_root: bool,
) -> Node {
    let an = arena.node(id);
    let name: Box<str> = arena.name(id).into();
    let kind = match an.kind() {
        ScannerKind::Directory => NodeKind::Directory,
        ScannerKind::File => NodeKind::File,
        ScannerKind::Symlink => NodeKind::Symlink,
        ScannerKind::Other => NodeKind::Other,
    };
    if kind == NodeKind::Directory {
        let mut children = Vec::new();
        for child_id in arena.children(id) {
            let child = arena.node(child_id);
            if !include_hidden && child.flags.has(NodeFlags::HIDDEN) {
                continue;
            }
            children.push(build_node(
                arena,
                child_id,
                apparent_size,
                include_hidden,
                link_map,
                false,
            ));
        }
        let mut node = Node::directory(name);
        node.read_error = an.read_error();
        node.modified = an.modified;
        node.children = children;
        let _ = is_root;
        node
    } else {
        let bytes = if apparent_size {
            an.apparent_bytes
        } else {
            an.allocated_bytes
        };
        let mut node = Node::entry(name, kind, bytes);
        node.modified = an.modified;
        node.read_error = an.read_error();
        node.inode = link_map.get(&id).copied();
        node
    }
}

/// Convert a [`Node`] tree back into a [`TreeArena`] (used when widening from
/// a pre-built tree or in tests).
#[must_use]
pub fn node_to_arena(root_path: PathBuf, root: &Node) -> TreeArena {
    node_to_arena_at(root_path, root, 0)
}

/// Convert a [`Node`] tree into a [`TreeArena`] and stamp every directory with
/// `scanned_at` (used when persisting completed remote SSH scans to SWR cache).
#[must_use]
pub fn node_to_arena_at(
    root_path: PathBuf,
    root: &Node,
    scanned_at: u64,
) -> TreeArena {
    let mut arena = TreeArena::new(root_path, root.name.as_ref());
    let root_id = arena.root();
    copy_node_into_arena(&mut arena, root_id, root, scanned_at);
    arena.aggregate(true, true);
    if scanned_at > 0 {
        arena.refresh_statuses(scanned_at, 1);
    }
    arena
}

fn copy_node_into_arena(
    arena: &mut TreeArena,
    parent: NodeId,
    node: &Node,
    scanned_at: u64,
) {
    {
        let target = arena.node_mut(parent);
        target.modified = node.modified;
        target.scanned_at = scanned_at;
        target.flags.set(NodeFlags::READ_ERROR, node.read_error);
        target
            .flags
            .set(NodeFlags::HIDDEN, node.name.starts_with('.'));
    }
    for child in &node.children {
        let kind = match child.kind {
            NodeKind::Directory => ScannerKind::Directory,
            NodeKind::File => ScannerKind::File,
            NodeKind::Symlink => ScannerKind::Symlink,
            NodeKind::Other => ScannerKind::Other,
        };
        let mut flags = NodeFlags::new(kind);
        flags.set(NodeFlags::READ_ERROR, child.read_error);
        flags.set(NodeFlags::HIDDEN, child.name.starts_with('.'));
        let bytes = if kind == ScannerKind::Directory {
            0
        } else {
            child.bytes
        };
        let id = arena.add_child(
            parent,
            child.name.as_ref(),
            kind,
            bytes,
            bytes,
            child.modified,
            flags,
        );
        if let Some((dev, ino)) = child.inode {
            arena.record_hardlink(id, dev, ino.get());
        }
        if kind == ScannerKind::Directory {
            copy_node_into_arena(arena, id, child, scanned_at);
        }
    }
}

/// Scan `root` synchronously via `disktree-scanner` and return a classified
/// [`Node`] tree.
pub fn scan(root: &Path, options: ScanOptions) -> io::Result<Node> {
    let scanner_opts = options.to_scanner();
    let outcome = walk_parallel(
        root,
        WalkConfig {
            options: &scanner_opts,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(WalkProgress::default()),
            pause_gate: None,
            skip_oracle: None,
            on_dir_start: None,
            on_dir_tally: None,
            on_dir_complete: None,
        },
    )?;
    Ok(arena_to_node(
        &outcome.arena,
        options.apparent_size,
        options.include_hidden,
        options.dedup_hardlinks,
        options.metric,
    ))
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use tempfile::TempDir;

    fn scan_dir(path: &Path, options: &ScanOptions) -> Node {
        scan(path, *options).expect("scan succeeds")
    }

    fn apparent() -> ScanOptions {
        ScanOptions {
            apparent_size: true,
            ..ScanOptions::default()
        }
    }

    #[test]
    fn totals_are_summed_bottom_up() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("a/b")).expect("mkdir");
        fs::write(temp.path().join("root.txt"), b"12345").expect("write");
        fs::write(temp.path().join("a/one.txt"), b"1234567890").expect("write");
        fs::write(temp.path().join("a/b/two.txt"), b"123").expect("write");

        let tree = scan_dir(temp.path(), &apparent());
        assert_eq!(tree.bytes, 18);
        assert_eq!(tree.files, 3);
        assert_eq!(tree.dirs, 3);
        let a = tree.child_named("a").expect("a");
        assert_eq!(a.bytes, 13);
        assert_eq!(a.files, 2);
        assert_eq!(a.dirs, 2);
    }

    #[test]
    fn a_directory_reports_its_own_bytes_separately() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("sub")).expect("mkdir");
        fs::write(temp.path().join("direct.bin"), [0_u8; 40]).expect("write");
        fs::write(temp.path().join("sub/nested.bin"), [0_u8; 60])
            .expect("write");

        let tree = scan_dir(temp.path(), &apparent());
        assert_eq!(tree.own_bytes(), 40);
        assert_eq!(tree.own_files(), 1);
        assert_eq!(tree.bytes, 100);
    }

    #[test]
    fn children_are_ranked_largest_first() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("small"), [0_u8; 10]).expect("write");
        fs::write(temp.path().join("huge"), [0_u8; 1000]).expect("write");
        fs::write(temp.path().join("medium"), [0_u8; 100]).expect("write");

        let tree = scan_dir(temp.path(), &apparent());
        let names: Vec<&str> =
            tree.children.iter().map(|c| c.name.as_ref()).collect();
        assert_eq!(names, ["huge", "medium", "small"]);
    }

    #[test]
    fn file_count_metric_ranks_by_entries() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("many")).expect("mkdir");
        for i in 0..5 {
            fs::write(temp.path().join(format!("many/{i}")), b"x")
                .expect("write");
        }
        fs::write(temp.path().join("big"), [0_u8; 10_000]).expect("write");

        let tree = scan_dir(
            temp.path(),
            &ScanOptions {
                metric: Metric::Files,
                ..apparent()
            },
        );
        assert_eq!(tree.children[0].name.as_ref(), "many");
        assert_eq!(tree.children[1].name.as_ref(), "big");
    }

    #[test]
    fn hardlinks_are_charged_once_by_default() {
        let temp = TempDir::new().expect("tempdir");
        let original = temp.path().join("original.bin");
        fs::write(&original, [0_u8; 4096]).expect("write");
        fs::hard_link(&original, temp.path().join("second.bin"))
            .expect("hard_link");

        let deduped = scan_dir(temp.path(), &apparent());
        assert_eq!(deduped.bytes, 4096);
        assert_eq!(deduped.files, 2);

        let raw = scan_dir(
            temp.path(),
            &ScanOptions {
                dedup_hardlinks: false,
                ..apparent()
            },
        );
        assert_eq!(raw.bytes, 8192);
    }

    #[test]
    fn hidden_entries_are_included_by_default_and_excluded_on_request() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join(".cache")).expect("mkdir");
        fs::write(temp.path().join(".cache/blob"), [0_u8; 500]).expect("write");
        fs::write(temp.path().join("visible"), [0_u8; 50]).expect("write");

        let with_hidden = scan_dir(temp.path(), &apparent());
        assert_eq!(with_hidden.bytes, 550);
        assert!(with_hidden.child_named(".cache").is_some());

        let without_hidden = scan_dir(
            temp.path(),
            &ScanOptions {
                include_hidden: false,
                ..apparent()
            },
        );
        assert_eq!(without_hidden.bytes, 50);
        assert!(without_hidden.child_named(".cache").is_none());
    }

    #[test]
    fn max_depth_stops_descending_but_keeps_direct_files() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("a/b/c")).expect("mkdir");
        fs::write(temp.path().join("root.txt"), [0_u8; 10]).expect("write");
        fs::write(temp.path().join("a/one.txt"), [0_u8; 20]).expect("write");
        fs::write(temp.path().join("a/b/two.txt"), [0_u8; 40]).expect("write");
        fs::write(temp.path().join("a/b/c/three.txt"), [0_u8; 80])
            .expect("write");

        let shallow = scan_dir(
            temp.path(),
            &ScanOptions {
                max_depth: Some(1),
                ..apparent()
            },
        );
        assert_eq!(shallow.bytes, 30);
        assert_eq!(shallow.files, 2);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed_by_default() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("outside");
        fs::write(outside.path().join("huge.bin"), [0_u8; 10_000])
            .expect("write");
        symlink(outside.path(), temp.path().join("escape")).expect("symlink");

        let safe = scan_dir(temp.path(), &apparent());
        assert!(safe.bytes < 10_000, "symlink target was counted");
        let link = safe.child_named("escape").expect("link");
        assert_eq!(link.kind, NodeKind::Symlink);

        let followed = scan_dir(
            temp.path(),
            &ScanOptions {
                follow_links: true,
                one_filesystem: false,
                ..apparent()
            },
        );
        assert_eq!(followed.bytes, 10_000);
    }

    #[cfg(unix)]
    #[test]
    fn followed_symlink_loops_do_not_hang_the_scan() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().expect("tempdir");
        fs::create_dir(temp.path().join("sub")).expect("mkdir");
        fs::write(temp.path().join("sub/file"), [0_u8; 100]).expect("write");
        symlink(temp.path(), temp.path().join("sub/back")).expect("symlink");

        let tree = scan_dir(
            temp.path(),
            &ScanOptions {
                follow_links: true,
                ..apparent()
            },
        );
        assert_eq!(tree.bytes, 100);
    }

    #[cfg(unix)]
    #[test]
    fn disk_usage_is_whole_allocation_units() {
        let temp = TempDir::new().expect("tempdir");
        // Incompressible bytes so a compressing filesystem still allocates blocks.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let noise: Vec<u8> = (0..100_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect();
        let mut file =
            fs::File::create(temp.path().join("data.bin")).expect("create");
        std::io::Write::write_all(&mut file, &noise).expect("write");
        file.sync_all().expect("sync");
        drop(file);
        let tree = scan_dir(temp.path(), &ScanOptions::default());
        assert!(tree.bytes >= 100_000, "{}", tree.bytes);
        assert_eq!(tree.bytes % 512, 0, "{}", tree.bytes);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_is_recorded_not_fatal() {
        use std::os::unix::fs::PermissionsExt as _;

        if rustix::process::geteuid().is_root() {
            return;
        }
        let temp = TempDir::new().expect("tempdir");
        let locked = temp.path().join("locked");
        fs::create_dir(&locked).expect("mkdir");
        fs::write(locked.join("secret"), b"nope").expect("write");
        fs::write(temp.path().join("open"), b"hello").expect("write");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
            .expect("chmod");

        let tree = scan_dir(temp.path(), &apparent());
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700))
            .expect("restore");

        assert_eq!(tree.bytes, 5);
        assert!(tree.child_named("locked").expect("locked").read_error);
    }

    #[test]
    fn a_file_root_is_rejected() {
        let temp = TempDir::new().expect("tempdir");
        let file = temp.path().join("file.txt");
        fs::write(&file, b"hi").expect("write");
        let err = scan(&file, apparent()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(feature = "cache-db")]
    #[test]
    fn partial_stub_from_narrow_child_is_never_emitted_as_parent_tree() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().join("home");
        let child = root.join("src/repo");
        fs::create_dir_all(&child).expect("mkdir");
        fs::write(child.join("code.rs"), [0_u8; 500]).expect("write");
        fs::write(root.join("big.bin"), vec![0_u8; 50_000]).expect("write");

        let db_path = temp.path().join("cache.redb");
        let policy = CachePolicy {
            min_free_headroom: 0,
            ..CachePolicy::default()
        };
        let scanner = Scanner::new(
            disktree_scanner::StorageMode::Persistent {
                path: db_path,
                strict: true,
            },
            policy,
        )
        .expect("scanner");

        // Warm the cache with ONLY the narrow child directory.
        let _ = scanner
            .scan_blocking(&child, apparent().to_scanner(), policy, false)
            .expect("narrow scan");

        // Now open a session on the parent `root`: `ScanHandle` must skip the
        // partial ancestor stub and only emit the complete Live tree.
        let handle =
            ScanHandle::spawn_session(&scanner, root, apparent(), policy)
                .expect("session");
        let mut updates = Vec::new();
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Some(update) = handle.poll_update() {
                let done = matches!(
                    update,
                    ScanUpdate::Live { .. } | ScanUpdate::Error(_)
                );
                updates.push(update);
                if done {
                    break;
                }
            } else {
                thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        assert!(
            !updates
                .iter()
                .any(|u| matches!(u, ScanUpdate::Cached { .. })),
            "partial stub must not be emitted as a cached parent tree"
        );
        let live = updates
            .into_iter()
            .find_map(|u| match u {
                ScanUpdate::Live { node, .. } => Some(node),
                _ => None,
            })
            .expect("complete live tree");
        assert!(live.child_named("big.bin").is_some());
        assert_eq!(live.bytes, 50_500);
    }

    #[test]
    fn a_spawned_scan_reports_progress_and_a_result() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("a"), [0_u8; 50]).expect("write");
        fs::write(temp.path().join("b"), [0_u8; 75]).expect("write");

        let handle = ScanHandle::spawn(temp.path().to_path_buf(), apparent());
        let tree = loop {
            if let Some(outcome) = handle.poll() {
                break outcome.expect("scan");
            }
            thread::sleep(std::time::Duration::from_millis(5));
        };
        assert_eq!(tree.bytes, 125);
        let snap = handle.progress.snapshot();
        assert!(snap.finished);
        assert_eq!(snap.files, 2);
    }

    #[test]
    fn a_wider_scan_reuses_the_subtree_it_already_knows() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path().canonicalize().expect("canonical");
        let inner = root.join("inner");
        fs::create_dir(&inner).expect("mkdir");
        fs::write(inner.join("a.bin"), [0_u8; 200]).expect("write");
        fs::write(root.join("outside.bin"), [0_u8; 50]).expect("write");

        let scanner_opts = apparent().to_scanner();
        let known_arena = Arc::new(
            walk_parallel(
                &inner,
                WalkConfig {
                    options: &scanner_opts,
                    cancel: Arc::new(AtomicBool::new(false)),
                    progress: Arc::new(WalkProgress::default()),
                    pause_gate: None,
                    skip_oracle: None,
                    on_dir_start: None,
                    on_dir_tally: None,
                    on_dir_complete: None,
                },
            )
            .expect("inner")
            .arena,
        );
        // Add a new file inside `inner` after the narrow scan: reusing `known`
        // proves `inner` was grafted rather than re-walked.
        fs::write(inner.join("b.bin"), [0_u8; 999]).expect("write");

        let handle = ScanHandle::spawn_with(
            root,
            apparent(),
            Some(Known {
                path: inner,
                arena: known_arena,
            }),
        );
        let tree = loop {
            if let Some(outcome) = handle.poll() {
                break outcome.expect("wider");
            }
            thread::sleep(std::time::Duration::from_millis(5));
        };
        let reused = tree.child_named("inner").expect("inner");
        assert_eq!(reused.bytes, 200, "b.bin was never read");
        assert_eq!(reused.files, 1);
        assert_eq!(tree.bytes, 250);
    }

    #[test]
    fn node_to_arena_roundtrips_remote_trees_for_swr_cache() {
        let mut root = Node::directory("dev-home");
        root.modified = 1_700_000_000;
        let mut sub = Node::directory(".cache");
        sub.read_error = true;
        sub.children
            .push(Node::entry("blob.bin", NodeKind::File, 4096));
        root.children.push(sub);
        root.children
            .push(Node::entry("readme.md", NodeKind::File, 512));
        aggregate(&mut root, Metric::Bytes);

        let arena =
            node_to_arena_at(PathBuf::from("/home/dev"), &root, 1_700_000_123);
        let roundtrip = arena_to_node(&arena, false, true, true, Metric::Bytes);
        assert_eq!(roundtrip.bytes, 4608);
        assert_eq!(roundtrip.files, 2);
        let cache = roundtrip.child_named(".cache").expect(".cache");
        assert!(cache.read_error);
        assert_eq!(cache.bytes, 4096);

        let without_hidden =
            arena_to_node(&arena, false, false, true, Metric::Bytes);
        assert_eq!(without_hidden.bytes, 512);
        assert!(without_hidden.child_named(".cache").is_none());
    }

    #[test]
    #[ignore = "walks the whole disk"]
    fn whole_disk_smoke() {
        let home = std::env::home_dir().expect("a home directory");
        let root = crate::space::volume_root_for(&home).expect("a volume root");
        let started = std::time::Instant::now();
        let tree = scan(&root, ScanOptions::default()).expect("scan");
        println!("root {} in {:.1?}", root.display(), started.elapsed());
        println!(
            "total {} files {}",
            crate::size::human_bytes(tree.bytes),
            tree.files
        );
        for child in tree.children.iter().take(14) {
            println!(
                "  {:>10}  {}",
                crate::size::human_bytes(child.bytes),
                child.name
            );
        }
    }
}

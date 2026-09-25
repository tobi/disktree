//! Parallel filesystem scanning.
//!
//! The shape follows dust, because that shape is the reason dust is fast:
//!
//! * one `rayon::scope` per root, so recursion depth is O(1) however deep the
//!   tree is,
//! * every directory is a `PendingDir` with an atomic completion counter and a
//!   `+1` sentinel, so a directory is only built once its own scan *and* all of
//!   its subdirectory tasks have finished,
//! * children are handed to the parent as finished `Node`s and sizes are
//!   aggregated bottom-up in one serial pass, which is also where hardlinks are
//!   de-duplicated.
//!
//! What is added on top of dust's approach: live progress counters the UI can
//! poll without locking, and cooperative cancellation so a re-scan can abandon
//! a walk of a large home directory instead of queueing behind it.

use std::fs::{self, DirEntry, Metadata};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;

use rayon::Scope;
use rustc_hash::FxHashSet;

use crate::tree::{Metric, Node, NodeKind, aggregate};

/// Errors kept verbatim before the list is truncated; the count keeps rising.
const MAX_ERROR_DETAIL: usize = 50;

/// How a scan measures and filters the tree.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Measure apparent length instead of allocated blocks. Apparent size is
    /// what `ls -l` shows; blocks are what the volume actually spends, which is
    /// what a disk-space tool normally wants.
    pub apparent_size: bool,
    /// Follow symlinks. Off by default: a home directory is full of links, and
    /// following them double-counts.
    pub follow_links: bool,
    /// Include dotfiles and dot-directories. On by default, like dust and `du`:
    /// `~/.cache` is frequently the largest directory in a home directory.
    pub include_hidden: bool,
    /// Stay on the root's volume: skip other disks, pseudo filesystems,
    /// network shares and automount points, but keep subvolumes of the same
    /// disk. See [`crate::space::foreign_mounts`]. On by default: a disk
    /// tool measures a disk, and its free-space meter only means anything
    /// for one volume.
    pub one_filesystem: bool,
    /// Stop descending past this depth. Totals below that depth are then
    /// unknown, which makes an overview scan of a huge tree cheap.
    pub max_depth: Option<usize>,
    /// Count a hardlinked file once instead of once per link.
    pub dedup_hardlinks: bool,
    /// Whether children are ranked by bytes or by file count.
    pub metric: Metric,
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
        }
    }
}

/// Counters a running scan publishes for the UI.
///
/// Plain relaxed atomics: this is a progress meter, not a synchronisation
/// point, and the UI reads a snapshot that is allowed to be a few entries
/// stale.
#[derive(Debug, Default)]
pub struct ScanProgress {
    files: AtomicU64,
    dirs: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    finished: AtomicBool,
    cancelled: AtomicBool,
    messages: Mutex<Vec<String>>,
}

/// A point-in-time view of [`ScanProgress`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScanSnapshot {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: u64,
    pub finished: bool,
    pub cancelled: bool,
    /// Up to [`MAX_ERROR_DETAIL`] unreadable paths, most recent last.
    pub messages: Vec<String>,
}

impl ScanProgress {
    fn count_file(&self, bytes: u64) {
        self.files.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    fn count_dir(&self) {
        self.dirs.fetch_add(1, Ordering::Relaxed);
    }

    fn record_error(&self, path: &Path, error: &io::Error) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        let message = format!("{}: {error}", path.display());
        let mut messages = lock(&self.messages);
        if messages.len() < MAX_ERROR_DETAIL {
            messages.push(message);
        }
        drop(messages);
    }

    fn finish(&self) {
        self.finished.store(true, Ordering::Relaxed);
    }

    /// Ask the walk to stop at the next directory boundary.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Stop the walk now, without waiting for the worker thread.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> ScanSnapshot {
        ScanSnapshot {
            files: self.files.load(Ordering::Relaxed),
            dirs: self.dirs.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            finished: self.finished.load(Ordering::Relaxed),
            cancelled: self.is_cancelled(),
            messages: lock(&self.messages).clone(),
        }
    }
}

/// A scan running on its own thread.
#[derive(Debug)]
pub struct ScanHandle {
    pub progress: Arc<ScanProgress>,
    result: Receiver<io::Result<Node>>,
}

/// A subtree already measured, reused by a wider scan instead of walked
/// again: widening from `~` to `/` only reads what is outside `~`.
#[derive(Clone, Debug)]
pub struct Known {
    /// Where it is. Compared with the walk's own paths, so give it in the
    /// same form as the root (both canonical).
    pub path: PathBuf,
    pub tree: Arc<Node>,
}

impl ScanHandle {
    /// Start walking `root` on a worker thread.
    pub fn spawn(root: PathBuf, options: ScanOptions) -> Self {
        Self::spawn_with(root, options, None)
    }

    /// Start walking `root`, reusing `known` where the walk reaches it.
    pub fn spawn_with(
        root: PathBuf,
        options: ScanOptions,
        known: Option<Known>,
    ) -> Self {
        let progress = Arc::new(ScanProgress::default());
        let context = Arc::new(WalkContext {
            known,
            options,
            progress: Arc::clone(&progress),
            root_device: Mutex::new(None),
            foreign_mounts: OnceLock::new(),
            visited_dirs: Mutex::new(FxHashSet::default()),
            root: Mutex::new(None),
        });
        let (sender, result) = mpsc::channel();

        // The closure owns the sender, so a failed spawn drops it and the
        // receiver reports the failure. Record the real reason alongside it.
        let worker = thread::Builder::new().name("disktree-scan".into()).spawn(
            move || {
                let outcome = scan_blocking(&root, &context);
                context.progress.finish();
                let _ = sender.send(outcome);
            },
        );
        if let Err(error) = worker {
            progress.record_error(Path::new("scan worker"), &error);
            progress.finish();
        }

        Self { progress, result }
    }

    /// Take the finished tree, if the walk is done.
    pub fn poll(&self) -> Option<io::Result<Node>> {
        match self.result.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(io::Error::other(
                "the scan worker stopped without a result",
            ))),
        }
    }

    /// Abandon the walk; the worker still finishes the directory it is in.
    pub fn cancel(&self) {
        self.progress.cancel();
    }
}

/// Walk `root` and return the aggregated tree. Blocking.
pub fn scan(root: &Path, options: ScanOptions) -> io::Result<Node> {
    let progress = Arc::new(ScanProgress::default());
    let context = Arc::new(WalkContext {
        known: None,
        options,
        progress: Arc::clone(&progress),
        root_device: Mutex::new(None),
        foreign_mounts: OnceLock::new(),
        visited_dirs: Mutex::new(FxHashSet::default()),
        root: Mutex::new(None),
    });
    let node = scan_blocking(root, &context)?;
    progress.finish();
    Ok(node)
}

struct WalkContext {
    /// A subtree to reuse rather than walk.
    known: Option<Known>,
    options: ScanOptions,
    progress: Arc<ScanProgress>,
    /// Device of the root, resolved once: the `one_filesystem` fallback
    /// when the mount table cannot be read.
    root_device: Mutex<Option<u64>>,
    /// Mount points `one_filesystem` keeps out, from the mount table. Unset
    /// when the table cannot be read, and devices are compared instead.
    foreign_mounts: OnceLock<FxHashSet<PathBuf>>,
    /// Directories already entered, so followed symlinks cannot loop.
    visited_dirs: Mutex<FxHashSet<(u64, u64)>>,
    /// Set by the root's `complete`, read after the scope joins.
    root: Mutex<Option<Node>>,
}

impl WalkContext {
    fn root_device(&self, path: &Path) -> Option<u64> {
        let mut cached = lock(&self.root_device);
        if let Some(device) = *cached {
            return Some(device);
        }
        let device = fs::metadata(path).map(|meta| device_of(&meta)).ok();
        *cached = device;
        device
    }

    fn cancelled(&self) -> bool {
        self.progress.is_cancelled()
    }

    /// Decide what to do with an entry, and account for the work it implies.
    fn classify(&self, entry: &DirEntry) -> Classified {
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                self.progress.record_error(&path, &error);
                return Classified::Skipped;
            }
        };
        // Non-UTF-8 names are lossy for display. The scan still measures them
        // correctly; only the reported name is approximate.
        let name: Box<str> =
            entry.file_name().to_string_lossy().into_owned().into();

        if !self.options.include_hidden && name.starts_with('.') {
            return Classified::Skipped;
        }

        if file_type.is_symlink() {
            return self.classify_symlink(&path, name);
        }

        if file_type.is_dir() {
            // Memoized: the subtree a narrower scan already measured is
            // taken whole, before any volume rule, since it was measured
            // under the same rules.
            if let Some(known) = &self.known
                && known.path == path
            {
                let tree = (*known.tree).clone();
                self.progress.files.fetch_add(tree.files, Ordering::Relaxed);
                self.progress.bytes.fetch_add(tree.bytes, Ordering::Relaxed);
                self.progress.dirs.fetch_add(tree.dirs, Ordering::Relaxed);
                let mut tree = tree;
                tree.name = name;
                return Classified::Entry(tree);
            }
            if self.options.one_filesystem
                && let Some(foreign) = self.foreign_mounts.get()
            {
                // Checked by path before anything reads the directory, so an
                // automount point is never triggered.
                if foreign.contains(&path) {
                    return Classified::Skipped;
                }
            } else if self.options.one_filesystem {
                let device = entry.metadata().map(|meta| device_of(&meta));
                match (device, self.root_device(&path)) {
                    (Ok(device), Some(root_device))
                        if device != root_device =>
                    {
                        return Classified::Skipped;
                    }
                    (Err(error), _) => {
                        self.progress.record_error(&path, &error);
                        return Classified::Skipped;
                    }
                    _ => {}
                }
            }
            self.progress.count_dir();
            return Classified::Subdirectory(path);
        }

        match entry.metadata() {
            Ok(meta) => self.leaf(name, kind_of(&meta, file_type), &meta),
            Err(error) => {
                self.progress.record_error(&path, &error);
                Classified::Skipped
            }
        }
    }

    fn classify_symlink(&self, path: &Path, name: Box<str>) -> Classified {
        if !self.options.follow_links {
            // Not followed: the link occupies only its target string, which
            // `du` reports as a handful of bytes or nothing at all.
            return match fs::symlink_metadata(path) {
                Ok(meta) => {
                    let size = measure(&meta, self.options.apparent_size);
                    self.progress.count_file(size);
                    Classified::Entry(leaf_node(
                        name,
                        NodeKind::Symlink,
                        size,
                        &meta,
                    ))
                }
                Err(error) => {
                    self.progress.record_error(path, &error);
                    Classified::Skipped
                }
            };
        }

        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) => {
                // A dangling link is normal, not an error worth counting.
                if error.kind() != io::ErrorKind::NotFound {
                    self.progress.record_error(path, &error);
                }
                return Classified::Skipped;
            }
        };

        if meta.is_dir() {
            if let Some(key) = file_identity(&meta)
                && !lock(&self.visited_dirs).insert(key)
            {
                return Classified::Skipped;
            }
            self.progress.count_dir();
            return Classified::Subdirectory(path.to_path_buf());
        }

        self.leaf(name, kind_of(&meta, meta.file_type()), &meta)
    }

    fn leaf(
        &self,
        name: Box<str>,
        kind: NodeKind,
        meta: &Metadata,
    ) -> Classified {
        let size = measure(meta, self.options.apparent_size);
        self.progress.count_file(size);
        Classified::Entry(leaf_node(name, kind, size, meta))
    }
}

/// What a directory entry turned out to be.
enum Classified {
    /// Descend into this directory on a new task.
    Subdirectory(PathBuf),
    /// A leaf that contributes size.
    Entry(Node),
    /// Filtered out, unreadable, or a symlink we chose not to follow.
    Skipped,
}

/// One directory being walked, plus the counter that decides when it is done.
#[derive(Debug)]
struct PendingDir {
    path: PathBuf,
    parent: Option<Arc<Self>>,
    /// Starts at 1 for the directory itself; one more per subdirectory task.
    /// When it reaches zero the directory is complete.
    pending: AtomicUsize,
    /// Files found directly here, and finished subdirectories handed back up.
    children: Mutex<Vec<Node>>,
    read_error: AtomicBool,
    depth: usize,
}

impl PendingDir {
    const fn new(
        path: PathBuf,
        parent: Option<Arc<Self>>,
        depth: usize,
    ) -> Self {
        Self {
            path,
            parent,
            pending: AtomicUsize::new(1),
            children: Mutex::new(Vec::new()),
            read_error: AtomicBool::new(false),
            depth,
        }
    }

    /// Turn a completed directory into a node. Only called when `pending` has
    /// reached zero, so every child is already in `self.children`.
    ///
    /// `bytes` and `own_bytes` are left at zero on purpose: the walk cannot
    /// know the aggregate, and [`crate::tree::aggregate`] derives both from the
    /// children once every child is present.
    fn build(&self) -> Node {
        Node {
            name: file_name(&self.path),
            kind: NodeKind::Directory,
            bytes: 0,
            own_bytes: 0,
            files: 0,
            own_files: 0,
            dirs: 1,
            inode: None,
            read_error: self.read_error.load(Ordering::Relaxed),
            modified: 0,
            category: crate::classify::Category::Other,
            reclaim: None,
            children: std::mem::take(&mut *lock(&self.children)),
        }
    }
}

fn scan_blocking(root: &Path, context: &Arc<WalkContext>) -> io::Result<Node> {
    let root_meta = fs::metadata(root)?;
    if !root_meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", root.display()),
        ));
    }
    if context.options.one_filesystem {
        *lock(&context.root_device) = Some(device_of(&root_meta));
        #[cfg(not(target_os = "macos"))]
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        #[cfg(target_os = "macos")]
        let foreign = crate::space::foreign_mounts_for(root);
        #[cfg(not(target_os = "macos"))]
        let foreign = crate::space::foreign_mounts_for(&root);
        if let Some(foreign) = foreign {
            let _ = context.foreign_mounts.set(foreign.into_iter().collect());
        }
    }
    if context.options.follow_links
        && let Some(key) = file_identity(&root_meta)
    {
        lock(&context.visited_dirs).insert(key);
    }

    let root_dir = Arc::new(PendingDir::new(root.to_path_buf(), None, 0));
    rayon::scope(|scope| walk(scope, &root_dir, context));

    let node = lock(&context.root).take();
    let node = node.ok_or_else(|| {
        io::Error::other(format!("{} produced no tree", root.display()))
    })?;
    Ok(finish_tree(node, &context.options))
}

/// Read one directory, spawn a task per subdirectory, then report completion.
///
/// Flat tasks inside the scope: the worker stack never grows with tree depth.
fn walk(scope: &Scope<'_>, dir: &Arc<PendingDir>, context: &Arc<WalkContext>) {
    let mut subdirs: Vec<Arc<PendingDir>> = Vec::new();
    let mut leaves: Vec<Node> = Vec::new();

    match fs::read_dir(&dir.path) {
        Ok(entries) => {
            for entry in entries {
                if context.cancelled() {
                    break;
                }
                match entry {
                    Ok(entry) => match context.classify(&entry) {
                        Classified::Subdirectory(path) => {
                            let child = Arc::new(PendingDir::new(
                                path,
                                Some(Arc::clone(dir)),
                                dir.depth + 1,
                            ));
                            subdirs.push(child);
                        }
                        Classified::Entry(node) => leaves.push(node),
                        Classified::Skipped => {}
                    },
                    Err(error) => {
                        context.progress.record_error(&dir.path, &error);
                    }
                }
            }
        }
        Err(error) => {
            context.progress.record_error(&dir.path, &error);
            dir.read_error.store(true, Ordering::Relaxed);
        }
    }

    // A depth-limited scan still measures what is directly in the directory,
    // it just does not descend further.
    let descend = context
        .options
        .max_depth
        .is_none_or(|max_depth| dir.depth < max_depth);
    if descend {
        for subdir in subdirs {
            if context.cancelled() {
                break;
            }
            dir.pending.fetch_add(1, Ordering::AcqRel);
            let subdir = Arc::clone(&subdir);
            let context = Arc::clone(context);
            scope.spawn(move |scope| walk(scope, &subdir, &context));
        }
    }
    lock(&dir.children).extend(leaves);

    signal_done(&Arc::clone(dir), context);
}

/// Report that one task for `dir` is done: either its own scan, or one of its
/// children. The last one to report builds the node and bubbles up.
///
/// This is the whole reason for the `+1` sentinel: a directory is only built
/// once its own scan *and* every subdirectory task has finished, no matter
/// which of them lands last.
fn signal_done(dir: &Arc<PendingDir>, context: &Arc<WalkContext>) {
    if dir.pending.fetch_sub(1, Ordering::AcqRel) != 1 {
        return;
    }
    let node = dir.build();
    let Some(parent) = dir.parent.clone() else {
        *lock(&context.root) = Some(node);
        return;
    };
    lock(&parent.children).push(node);
    signal_done(&parent, context);
}

/// Charge a hardlinked file once, then derive every aggregate from the result.
///
/// Zeroing `own_bytes` rather than `bytes` is deliberate:
/// [`crate::tree::aggregate`] recomputes totals from the direct contents, so a
/// patched `bytes` would be overwritten.
fn finish_tree(mut node: Node, options: &ScanOptions) -> Node {
    if options.dedup_hardlinks {
        let mut seen = FxHashSet::default();
        mark_duplicate_hardlinks(&mut node, &mut seen);
    }
    aggregate(&mut node, options.metric);
    crate::classify::classify(&mut node);
    node
}

fn mark_duplicate_hardlinks(node: &mut Node, seen: &mut FxHashSet<(u64, u64)>) {
    if !node.is_dir() {
        if node.inode.is_some_and(|key| !seen.insert(key)) {
            node.own_bytes = 0;
        }
        return;
    }
    for child in &mut node.children {
        mark_duplicate_hardlinks(child, seen);
    }
}

fn leaf_node(
    name: Box<str>,
    kind: NodeKind,
    size: u64,
    meta: &Metadata,
) -> Node {
    let mut node = Node::entry(name, kind, size);
    node.inode = file_identity(meta);
    node.modified = modified_seconds(meta);
    node
}

/// Last write time in Unix seconds, from the metadata the walk already has:
/// age costs no extra system call.
fn modified_seconds(meta: &Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| {
            i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
        })
}

fn measure(meta: &Metadata, apparent_size: bool) -> u64 {
    if apparent_size {
        meta.len()
    } else {
        allocated_bytes(meta).unwrap_or(meta.len())
    }
}

/// Allocated bytes, `st_blocks * 512`: sparse files spend less than they claim,
/// and this is the number that matches `du`.
///
/// Returns `Option` because only Unix reports allocated blocks; elsewhere the
/// caller falls back to the apparent length.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the Option is the non-Unix answer, where the caller falls back"
)]
#[cfg(unix)]
fn allocated_bytes(meta: &Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    Some(meta.blocks().saturating_mul(512))
}

#[cfg(not(unix))]
fn allocated_bytes(_meta: &Metadata) -> Option<u64> {
    None
}

#[cfg(unix)]
fn device_of(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.dev()
}

#[cfg(not(unix))]
fn device_of(_meta: &Metadata) -> u64 {
    0
}

/// `(device, inode)`, or `None` where the platform does not expose them.
/// Hardlink de-duplication and symlink loop detection both depend on this, so
/// they are simply unavailable rather than silently wrong elsewhere.
#[allow(
    clippy::unnecessary_wraps,
    reason = "the Option is the non-Unix answer, so both arms must agree"
)]
#[cfg(unix)]
fn file_identity(meta: &Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn file_identity(_meta: &Metadata) -> Option<(u64, u64)> {
    None
}

fn kind_of(meta: &Metadata, file_type: fs::FileType) -> NodeKind {
    if meta.is_dir() || file_type.is_dir() {
        NodeKind::Directory
    } else if meta.is_file() || file_type.is_file() {
        NodeKind::File
    } else if file_type.is_symlink() {
        NodeKind::Symlink
    } else {
        NodeKind::Other
    }
}

/// The display name of a scanned root: its final component, or the path itself
/// for `/`.
fn file_name(path: &Path) -> Box<str> {
    path.file_name()
        .map_or_else(
            || path.to_string_lossy().into_owned(),
            |name| name.to_string_lossy().into_owned(),
        )
        .into()
}

/// A poisoned lock means another task panicked; the data is still a valid tree
/// prefix, and refusing to read it would turn one panic into two.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::Metric;
    use std::fs;
    use tempfile::TempDir;

    /// Apparent sizes, so the assertions are about the tree rather than about
    /// how the filesystem rounds a small file up to a block.
    fn options() -> ScanOptions {
        ScanOptions {
            apparent_size: true,
            ..ScanOptions::default()
        }
    }

    fn write(root: &Path, relative: &str, bytes: usize) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, vec![b'x'; bytes]).expect("write");
        path
    }

    fn scan_dir(root: &Path, options: &ScanOptions) -> Node {
        scan(root, options.clone()).expect("scan")
    }

    fn child<'a>(node: &'a Node, name: &str) -> &'a Node {
        node.children
            .iter()
            .find(|child| &*child.name == name)
            .unwrap_or_else(|| {
                panic!("no child named {name} in {:?}", node.name)
            })
    }

    #[test]
    fn totals_are_summed_bottom_up() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        write(root, "a/one.bin", 1000);
        write(root, "a/nested/two.bin", 2000);
        write(root, "b/three.bin", 500);

        let tree = scan_dir(root, &options());
        assert_eq!(tree.bytes, 3500);
        assert_eq!(tree.files, 3);
        assert_eq!(tree.dirs, 4, "root, a, a/nested, b");
        assert_eq!(child(&tree, "a").bytes, 3000);
        assert_eq!(child(child(&tree, "a"), "nested").bytes, 2000);
        assert_eq!(child(&tree, "b").bytes, 500);
    }

    #[test]
    fn children_are_ranked_largest_first() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        write(root, "small.bin", 10);
        write(root, "large.bin", 1000);
        write(root, "medium.bin", 100);

        let tree = scan_dir(root, &options());
        let names: Vec<&str> = tree.children.iter().map(|c| &*c.name).collect();
        assert_eq!(names, vec!["large.bin", "medium.bin", "small.bin"]);
    }

    #[test]
    fn a_directory_reports_its_own_bytes_separately() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        write(root, "direct.bin", 700);
        write(root, "sub/deep.bin", 300);

        let tree = scan_dir(root, &options());
        assert_eq!(tree.own_bytes, 700);
        assert_eq!(tree.own_files, 1);
        assert_eq!(tree.bytes, 1000);
        assert_eq!(tree.files, 2);
    }

    #[test]
    fn hardlinks_are_charged_once_by_default() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        let original = write(root, "original.bin", 4096);
        fs::hard_link(&original, root.join("link.bin")).expect("hard link");

        let deduped = scan_dir(root, &options());
        assert_eq!(deduped.bytes, 4096);
        assert_eq!(deduped.files, 2, "both names still exist");

        let counted = scan_dir(
            root,
            &ScanOptions {
                dedup_hardlinks: false,
                ..options()
            },
        );
        assert_eq!(counted.bytes, 8192);
    }

    #[test]
    fn hidden_entries_are_included_by_default_and_excluded_on_request() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        write(root, ".cache/blob.bin", 900);
        write(root, "visible.bin", 100);

        let default = scan_dir(root, &options());
        assert_eq!(default.bytes, 1000);
        assert_eq!(child(&default, ".cache").bytes, 900);

        let without = scan_dir(
            root,
            &ScanOptions {
                include_hidden: false,
                ..options()
            },
        );
        assert_eq!(without.bytes, 100);
    }

    #[test]
    fn symlinks_are_not_followed_by_default() {
        let temp = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("tempdir");
        write(outside.path(), "elsewhere.bin", 5000);
        let root = temp.path();
        write(root, "real.bin", 100);
        std::os::unix::fs::symlink(outside.path(), root.join("link"))
            .expect("symlink");

        let tree = scan_dir(root, &options());
        // Only the real file's bytes; the link itself holds just its target
        // string, and its 5000-byte destination is outside this tree.
        assert!(tree.bytes < 200, "{}", tree.bytes);
        let link = child(&tree, "link");
        assert_eq!(link.kind, NodeKind::Symlink);
        assert!(link.bytes < 100, "a link holds only its target string");
    }

    #[test]
    fn followed_symlink_loops_do_not_hang_the_scan() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        write(root, "sub/leaf.bin", 42);
        std::os::unix::fs::symlink(root, root.join("sub/loop"))
            .expect("symlink");

        let tree = scan_dir(
            root,
            &ScanOptions {
                follow_links: true,
                ..options()
            },
        );
        assert_eq!(tree.bytes, 42);
    }

    #[test]
    fn max_depth_stops_descending_but_keeps_direct_files() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        write(root, "here.bin", 10);
        write(root, "a/there.bin", 20);
        write(root, "a/b/far.bin", 30);

        let tree = scan_dir(
            root,
            &ScanOptions {
                max_depth: Some(1),
                ..options()
            },
        );
        assert_eq!(tree.own_bytes, 10);
        let a = child(&tree, "a");
        assert_eq!(
            a.bytes, 20,
            "direct files at the cut-off depth still count"
        );
        assert!(
            a.children.iter().all(|child| !child.is_dir()),
            "descending stopped at depth 1"
        );
        assert_eq!(a.files, 1);
    }

    #[test]
    fn file_count_metric_ranks_by_entries() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        for index in 0..5 {
            write(root, &format!("many/f{index}.bin"), 1);
        }
        write(root, "one/huge.bin", 100_000);

        let tree = scan_dir(root, &options());
        assert_eq!(child(&tree, "one").bytes, 100_000);
        assert_eq!(tree.children[0].name.as_ref(), "one");

        let by_files = scan_dir(
            root,
            &ScanOptions {
                metric: Metric::Files,
                ..options()
            },
        );
        assert_eq!(by_files.children[0].name.as_ref(), "many");
        assert_eq!(by_files.children[0].files, 5);
    }

    #[test]
    fn an_unreadable_directory_is_recorded_not_fatal() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        write(root, "readable.bin", 11);
        let locked = root.join("locked");
        fs::create_dir_all(&locked).expect("mkdir");
        write(&locked, "hidden.bin", 22);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
            .expect("chmod");

        // Running as root would bypass the permission bits entirely.
        let readable = fs::read_dir(&locked).map(|mut it| it.next().is_some());
        let progress = Arc::new(ScanProgress::default());
        let context = Arc::new(WalkContext {
            known: None,
            options: options(),
            progress: Arc::clone(&progress),
            root_device: Mutex::new(None),
            foreign_mounts: OnceLock::new(),
            visited_dirs: Mutex::new(FxHashSet::default()),
            root: Mutex::new(None),
        });
        let tree = scan_blocking(root, &context).expect("scan still succeeds");
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o700));

        assert_eq!(tree.bytes, 11);
        let node = child(&tree, "locked");
        if matches!(readable, Ok(false)) {
            assert!(node.read_error);
            assert!(progress.snapshot().errors >= 1);
            assert!(!progress.snapshot().messages.is_empty());
        }
    }

    #[test]
    fn a_file_root_is_rejected() {
        let temp = TempDir::new().expect("tempdir");
        let path = write(temp.path(), "file.bin", 1);
        let error = scan(&path, options()).expect_err("a file is not a tree");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_spawned_scan_reports_progress_and_a_result() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        write(root, "a/one.bin", 1024);
        write(root, "b/two.bin", 2048);

        let handle = ScanHandle::spawn(root.to_path_buf(), options());
        let mut outcome = None;
        for _ in 0..2000 {
            if let Some(result) = handle.poll() {
                outcome = Some(result);
                break;
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
        let tree = outcome.expect("the scan finished").expect("a tree");
        assert_eq!(tree.bytes, 3072);

        let snapshot = handle.progress.snapshot();
        assert!(snapshot.finished);
        assert_eq!(snapshot.files, 2);
        assert_eq!(snapshot.errors, 0);
    }

    #[test]
    fn a_wider_scan_reuses_the_subtree_it_already_knows() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().canonicalize().expect("canonical");
        let inner = root.join("inner");
        fs::create_dir_all(inner.join("deep")).expect("mkdir");
        fs::write(inner.join("deep/a.bin"), vec![0_u8; 4096]).expect("write");
        fs::write(root.join("outside.bin"), vec![0_u8; 8192]).expect("write");
        let known = scan(&inner, options()).expect("inner scan");

        // Changed on disk after the inner scan: a memoized subtree must not
        // see it, which is how this test knows the walk skipped it.
        fs::write(inner.join("deep/b.bin"), vec![0_u8; 4096]).expect("write");

        let handle = ScanHandle::spawn_with(
            root,
            options(),
            Some(Known {
                path: inner,
                tree: Arc::new(known.clone()),
            }),
        );
        let tree = loop {
            if let Some(outcome) = handle.poll() {
                break outcome.expect("wide scan");
            }
            thread::sleep(std::time::Duration::from_millis(2));
        };
        let reused = tree.child_named("inner").expect("inner is in the tree");
        assert_eq!(reused.files, known.files, "b.bin was never read");
        assert_eq!(reused.bytes, known.bytes);
        assert!(
            tree.child_named("outside.bin").is_some(),
            "the rest was walked"
        );
        assert_eq!(tree.files, known.files + 1);
    }

    /// A real whole-disk scan, run by hand: `cargo test -p disktree-core
    /// -- --ignored --nocapture whole_disk`. Prints what it found, so the
    /// volume rules can be checked against this machine's mounts.
    #[test]
    #[ignore = "walks the whole disk"]
    fn whole_disk_smoke() {
        let home = std::env::var_os("HOME").map(PathBuf::from).expect("HOME");
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
        let skipped =
            crate::space::foreign_mounts_for(&root).unwrap_or_default();
        println!("left out: {skipped:?}");
        for needle in ["c", "cache", "node_modules", "zzzz"] {
            let started = std::time::Instant::now();
            let found =
                crate::filter::filter(&tree, &[], needle).expect("needle");
            println!(
                "filter {needle:?}: {} matches, {} in {:.1?}",
                found.count,
                crate::size::human_bytes(found.bytes),
                started.elapsed()
            );
        }
    }
}

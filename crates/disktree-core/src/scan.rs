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

#[cfg(not(windows))]
use std::fs::DirEntry;
use std::fs::{self, Metadata};
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
        // A scan without Full Disk Access can refuse thousands of paths;
        // only the ones that will be kept are worth formatting.
        let mut messages = lock(&self.messages);
        if messages.len() < MAX_ERROR_DETAIL {
            messages.push(format!("{}: {error}", path.display()));
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
            never_scanned: OnceLock::new(),
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
        never_scanned: OnceLock::new(),
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
    /// Directories no scan enters, on any volume setting: see
    /// [`crate::space::never_scanned`].
    never_scanned: OnceLock<FxHashSet<PathBuf>>,
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
    fn classify(&self, entry: &impl Listed) -> Classified {
        let path = entry.entry_path();
        let listing = match entry.listing() {
            Ok(listing) => listing,
            Err(error) => {
                self.progress.record_error(&path, &error);
                return Classified::Skipped;
            }
        };
        let name = entry.display_name();

        if !self.options.include_hidden
            && (name.starts_with('.') || entry.hidden())
        {
            return Classified::Skipped;
        }

        let kind = match listing {
            Listing::Symlink => return self.classify_symlink(&path, name),
            Listing::Directory => return self.classify_dir(entry, path, name),
            Listing::Leaf(kind) => kind,
        };
        match entry.facts(self.options.apparent_size) {
            Ok(facts) => self.leaf(name, kind, &facts),
            Err(error) => {
                self.progress.record_error(&path, &error);
                Classified::Skipped
            }
        }
    }

    fn classify_dir(
        &self,
        entry: &impl Listed,
        path: PathBuf,
        name: Box<str>,
    ) -> Classified {
        if self
            .never_scanned
            .get()
            .is_some_and(|never| never.contains(&path))
        {
            return Classified::Skipped;
        }
        // Read at most once per directory: macOS needs it for the dataless
        // flag, and the device check below wherever the mount table cannot
        // be read, which on macOS is everywhere.
        let directory_cell = std::cell::OnceCell::new();
        let directory = || directory_cell.get_or_init(|| entry.directory());
        if EVICTED_IS_KNOWN
            && directory()
                .as_ref()
                .is_ok_and(|directory| directory.evicted)
        {
            return Classified::Skipped;
        }
        // Memoized: the subtree a narrower scan already measured is taken
        // whole, before any volume rule, since it was measured under the same
        // rules.
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
            match (directory(), self.root_device(&path)) {
                (Ok(directory), Some(root_device))
                    if directory.device != root_device =>
                {
                    return Classified::Skipped;
                }
                (Err(error), _) => {
                    self.progress.record_error(&path, error);
                    return Classified::Skipped;
                }
                _ => {}
            }
        }
        self.progress.count_dir();
        Classified::Subdirectory(path)
    }

    fn classify_symlink(&self, path: &Path, name: Box<str>) -> Classified {
        if !self.options.follow_links {
            // Not followed: the link occupies only its target string, which
            // `du` reports as a handful of bytes or nothing at all.
            return match fs::symlink_metadata(path) {
                Ok(meta) => {
                    let facts = Facts::of(&meta, self.options.apparent_size);
                    self.progress.count_file(facts.size);
                    Classified::Entry(leaf_node(
                        name,
                        NodeKind::Symlink,
                        &facts,
                        facts.shared,
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
            // A link into an evicted cloud folder is left alone for the
            // same reason the folder itself is.
            if is_dataless(&meta) {
                return Classified::Skipped;
            }
            if let Some(key) = identity_of(path, &meta)
                && !lock(&self.visited_dirs).insert(key)
            {
                return Classified::Skipped;
            }
            self.progress.count_dir();
            return Classified::Subdirectory(path.to_path_buf());
        }

        let mut facts = Facts::of(&meta, self.options.apparent_size);
        // What the link leads to, so a file that is also reached directly is
        // charged once.
        facts.identity = identity_of(path, &meta);
        self.leaf(name, kind_of(&meta, meta.file_type()), &facts)
    }

    fn leaf(
        &self,
        name: Box<str>,
        kind: NodeKind,
        facts: &Facts,
    ) -> Classified {
        self.progress.count_file(facts.size);
        // A followed link reaches a file a second way, whatever its link
        // count says.
        let track = self.options.follow_links || facts.shared;
        Classified::Entry(leaf_node(name, kind, facts, track))
    }
}

/// What the walk reads about one directory entry.
///
/// `std::fs::DirEntry` everywhere but Windows. There the standard listing
/// has neither the allocated size nor a file id, so measuring like `du`
/// would cost an open per file; [`crate::windows`] lists a directory with
/// both instead.
trait Listed {
    fn entry_path(&self) -> PathBuf;
    /// Non-UTF-8 names are lossy for display. The scan still measures them
    /// correctly; only the reported name is approximate.
    fn display_name(&self) -> Box<str>;
    fn listing(&self) -> io::Result<Listing>;
    /// Size, identity and age of a leaf.
    fn facts(&self, apparent_size: bool) -> io::Result<Facts>;
    /// What a directory is, read before the walk enters it.
    fn directory(&self) -> io::Result<Directory>;
    /// Hidden by an attribute rather than by a leading dot: on Windows,
    /// where the listing carries it, so `AppData` is hidden as Explorer
    /// hides it. macOS's `UF_HIDDEN` would cost a stat per entry.
    fn hidden(&self) -> bool {
        false
    }
}

/// Whether [`Directory::evicted`] can ever be true here, so a platform that
/// cannot say never pays for asking.
const EVICTED_IS_KNOWN: bool = cfg!(any(target_os = "macos", windows));

/// What the walk needs to know about a directory before entering it.
struct Directory {
    /// The device it is on, for staying on one volume when the mount table
    /// cannot be read.
    device: u64,
    /// Its contents live only in the cloud (iCloud Drive or a File Provider
    /// on macOS, `OneDrive` or another cloud files provider on Windows).
    /// Listing it would make the provider fetch them, so a disk scan must
    /// not; it holds no local blocks, so skipping it loses nothing.
    evicted: bool,
}

/// What an entry is, as far as the walk is concerned.
enum Listing {
    Directory,
    Symlink,
    Leaf(NodeKind),
}

/// What a leaf contributes.
struct Facts {
    size: u64,
    identity: Option<(u64, u64)>,
    modified: i64,
    /// The file may have another name the walk could meet: its identity is
    /// worth keeping for hardlink de-duplication.
    shared: bool,
}

impl Facts {
    fn of(meta: &Metadata, apparent_size: bool) -> Self {
        Self {
            size: measure(meta, apparent_size),
            identity: file_identity(meta),
            modified: modified_seconds(meta),
            shared: shares_inode(meta),
        }
    }
}

#[cfg(not(windows))]
impl Listed for DirEntry {
    fn entry_path(&self) -> PathBuf {
        self.path()
    }

    fn display_name(&self) -> Box<str> {
        self.file_name().to_string_lossy().into_owned().into()
    }

    fn listing(&self) -> io::Result<Listing> {
        let file_type = self.file_type()?;
        Ok(if file_type.is_symlink() {
            Listing::Symlink
        } else if file_type.is_dir() {
            Listing::Directory
        } else if file_type.is_file() {
            Listing::Leaf(NodeKind::File)
        } else {
            Listing::Leaf(NodeKind::Other)
        })
    }

    fn facts(&self, apparent_size: bool) -> io::Result<Facts> {
        self.metadata().map(|meta| Facts::of(&meta, apparent_size))
    }

    fn directory(&self) -> io::Result<Directory> {
        self.metadata().map(|meta| Directory {
            device: device_of(&meta),
            evicted: is_dataless(&meta),
        })
    }
}

#[cfg(windows)]
impl Listed for crate::windows::Entry {
    fn entry_path(&self) -> PathBuf {
        self.path()
    }

    fn display_name(&self) -> Box<str> {
        self.file_name().to_string_lossy().into_owned().into()
    }

    fn listing(&self) -> io::Result<Listing> {
        Ok(match self.kind() {
            crate::windows::Kind::Link => Listing::Symlink,
            crate::windows::Kind::Directory => Listing::Directory,
            crate::windows::Kind::File => Listing::Leaf(NodeKind::File),
        })
    }

    fn facts(&self, apparent_size: bool) -> io::Result<Facts> {
        Ok(Facts {
            size: if apparent_size {
                self.apparent()
            } else {
                self.allocated()
            },
            identity: self.identity(),
            modified: self.modified(),
            // The listing has no link count; a file id is free here, so
            // every file keeps one.
            shared: true,
        })
    }

    /// The device is never consulted: see
    /// [`crate::space::foreign_mounts_for`]. The same answer as
    /// [`device_of`], so the two could only ever agree.
    fn directory(&self) -> io::Result<Directory> {
        Ok(Directory {
            device: 0,
            evicted: self.evicted(),
        })
    }

    fn hidden(&self) -> bool {
        self.hidden()
    }
}

/// List a directory the way [`Listed`] describes.
#[cfg(not(windows))]
fn list(path: &Path) -> io::Result<fs::ReadDir> {
    fs::read_dir(path)
}

#[cfg(windows)]
fn list(path: &Path) -> io::Result<crate::windows::ReadDir> {
    crate::windows::read_dir(path)
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
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut never = crate::space::never_scanned(root, &canonical);
    never.extend(crate::space::repeated_mounts_for(root, &canonical));
    let _ = context.never_scanned.set(never.into_iter().collect());
    if context.options.one_filesystem {
        *lock(&context.root_device) = Some(device_of(&root_meta));
        if let Some(foreign) = crate::space::foreign_mounts_for(&canonical) {
            let _ = context.foreign_mounts.set(foreign.into_iter().collect());
        }
    }
    if context.options.follow_links
        && let Some(key) = identity_of(root, &root_meta)
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

    match list(&dir.path) {
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

/// A leaf. Its identity is kept only when `track` says the walk could meet
/// the same file again: a hardlink de-duplication set of every file on a
/// disk is millions of entries, nearly all for files with one name.
fn leaf_node(
    name: Box<str>,
    kind: NodeKind,
    facts: &Facts,
    track: bool,
) -> Node {
    let mut node = Node::entry(name, kind, facts.size);
    if track {
        node.inode = facts.identity;
    }
    node.modified = facts.modified;
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
const fn allocated_bytes(_meta: &Metadata) -> Option<u64> {
    None
}

/// Whether a directory's contents are only in the cloud: an iCloud Drive or
/// File Provider folder macOS has evicted. Listing one makes macOS fetch its
/// entries from the server, so a disk scan must not; the folder holds no
/// local blocks, so skipping it loses nothing a scan measures.
///
/// A `stat` or `lstat` carries the flag without touching the contents.
#[cfg(target_os = "macos")]
fn is_dataless(meta: &Metadata) -> bool {
    use std::os::macos::fs::MetadataExt as _;
    // `SF_DATALESS` from <sys/stat.h>; the libc crate does not name it.
    const SF_DATALESS: u32 = 0x4000_0000;
    meta.st_flags() & SF_DATALESS != 0
}

#[cfg(not(target_os = "macos"))]
const fn is_dataless(_meta: &Metadata) -> bool {
    false
}

/// Whether the file has more than one name, so the walk can count it twice.
#[cfg(unix)]
fn shares_inode(meta: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    meta.nlink() > 1
}

#[cfg(not(unix))]
const fn shares_inode(_meta: &Metadata) -> bool {
    false
}

#[cfg(unix)]
fn device_of(meta: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.dev()
}

#[cfg(not(unix))]
const fn device_of(_meta: &Metadata) -> u64 {
    0
}

/// `(device, inode)`, or `None` where the platform does not expose them.
/// Hardlink de-duplication and symlink loop detection both depend on this, so
/// they are simply unavailable rather than silently wrong elsewhere. Windows
/// has no file id in `Metadata` on stable Rust; its listing carries one
/// instead (see [`crate::windows`]).
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
const fn file_identity(_meta: &Metadata) -> Option<(u64, u64)> {
    None
}

/// The identity of what `path` leads to, `meta` being its followed
/// metadata: how a walk that follows links recognizes a directory it has
/// entered before, and a file it has already charged.
#[cfg(not(windows))]
fn identity_of(_path: &Path, meta: &Metadata) -> Option<(u64, u64)> {
    file_identity(meta)
}

#[cfg(windows)]
fn identity_of(path: &Path, _meta: &Metadata) -> Option<(u64, u64)> {
    crate::windows::identity(path)
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

    /// Explorer's hidden attribute counts as hidden, as `AppData` has it.
    #[cfg(windows)]
    #[test]
    fn the_hidden_attribute_hides_an_entry_on_windows() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        write(root, "AppData/blob.bin", 900);
        write(root, "visible.bin", 100);
        let marked = std::process::Command::new("attrib")
            .arg("+h")
            .arg(root.join("AppData"))
            .status()
            .is_ok_and(|status| status.success());
        assert!(marked, "attrib +h");

        let without = scan_dir(
            root,
            &ScanOptions {
                include_hidden: false,
                ..options()
            },
        );
        assert_eq!(without.bytes, 100);
    }

    /// A link to a directory: a symbolic link on Unix, a junction on
    /// Windows, which needs no privilege to make. `false` if it could not be
    /// made.
    fn link_dir(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            crate::windows::make_junction(link, target)
        }
    }

    #[test]
    fn symlinks_are_not_followed_by_default() {
        let temp = TempDir::new().expect("tempdir");
        let outside = TempDir::new().expect("tempdir");
        write(outside.path(), "elsewhere.bin", 5000);
        let root = temp.path();
        write(root, "real.bin", 100);
        assert!(link_dir(outside.path(), &root.join("link")), "a link");

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
        assert!(link_dir(root, &root.join("sub/loop")), "a link");

        let tree = scan_dir(
            root,
            &ScanOptions {
                follow_links: true,
                ..options()
            },
        );
        assert_eq!(tree.bytes, 42);
    }

    /// Disk usage is what the volume allocated, not the length: whole
    /// blocks, so it covers the data and ends on a block boundary.
    #[test]
    fn disk_usage_is_whole_allocation_units() {
        let temp = TempDir::new().expect("tempdir");
        // Noise, so a compressing filesystem cannot store it in less.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let noise: Vec<u8> = (0..100_000)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect();
        fs::write(temp.path().join("data.bin"), noise).expect("write");
        let tree = scan_dir(temp.path(), &ScanOptions::default());
        assert!(tree.bytes >= 100_000, "{}", tree.bytes);
        assert_eq!(tree.bytes % 512, 0, "{}", tree.bytes);
        let apparent = scan_dir(temp.path(), &options());
        assert_eq!(apparent.bytes, 100_000);
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

    #[cfg(unix)]
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
            never_scanned: OnceLock::new(),
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

//! Parallel filesystem scanning.
//!
//! The shape follows dust, because that shape is the reason dust is fast:
//!
//! * one `rayon::scope` per root, so recursion depth is O(1) however deep the
//!   tree is,
//! * every directory is a `PendingDir` with an atomic completion counter and a
//!   `+1` sentinel, so a directory is only built once its own scan *and* all of
//!   its subdirectory tasks have finished,
//! * a finished directory totals and orders its entries and hands them to
//!   the tree being built ([`crate::tree`]), then its totals to its
//!   parent; a hardlinked file is charged by the first name listed.
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
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;

use rayon::Scope;
use rustc_hash::FxHashSet;

use crate::tree::{
    Builder, DIRECTORY, Dir, IDENTIFIED, Item, Metric, NONE, NodeKind,
    READ_ERROR, Seen, Totals, Tree, name_in, order, seconds,
};

#[cfg(windows)]
#[path = "walk_cache.rs"]
mod walk_cache;

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
    /// A directory the scan may keep what it read in, so the next scan of
    /// the same volume starts from it; `None` keeps nothing. NTFS folder
    /// walks use their own tree snapshot and the unprivileged journal.
    pub cache: Option<PathBuf>,
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
            cache: None,
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
    /// What a directory's listing found (see `Tally`), or what a reader
    /// that measures in bulk has come across so far.
    pub(crate) fn add(&self, files: u64, dirs: u64, bytes: u64) {
        self.files.fetch_add(files, Ordering::Relaxed);
        self.dirs.fetch_add(dirs, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// The totals of the finished tree, replacing a bulk reader's running
    /// count: it met the whole volume, and the tree may be less of it.
    #[cfg(windows)]
    pub(crate) fn settle(&self, files: u64, dirs: u64, bytes: u64) {
        self.files.store(files, Ordering::Relaxed);
        self.dirs.store(dirs, Ordering::Relaxed);
        self.bytes.store(bytes, Ordering::Relaxed);
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
    result: Receiver<io::Result<Arc<Tree>>>,
}

/// A subtree already measured, reused by a wider scan instead of walked
/// again: widening from `~` to `/` only reads what is outside `~`.
#[derive(Clone, Debug)]
pub struct Known {
    /// Where it is. Compared with the walk's own paths, so give it in the
    /// same form as the root (both canonical).
    pub path: PathBuf,
    pub tree: Arc<Tree>,
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
        let context =
            Arc::new(WalkContext::new(known, options, Arc::clone(&progress)));
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
    pub fn poll(&self) -> Option<io::Result<Arc<Tree>>> {
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

/// A kept tree being written, which a scan waits for before it reads one.
#[cfg(windows)]
static SAVING: Mutex<Option<thread::JoinHandle<()>>> = Mutex::new(None);

/// Wait until the last scan's kept tree is on disk: a scan hands its tree
/// over first and keeps it for the next on a thread of its own. See
/// [`ScanOptions::cache`].
#[cfg_attr(
    not(windows),
    allow(
        clippy::missing_const_for_fn,
        reason = "empty where nothing is saved in the background, but one \
                  signature on every platform"
    )
)]
pub fn wait_for_cache() {
    #[cfg(windows)]
    {
        let saving = lock(&SAVING).take();
        if let Some(saving) = saving {
            let _ = saving.join();
        }
    }
}

/// Keep a tree for the next scan with `work`, on a thread of its own once
/// any save before it is done: the scan hands its tree over without
/// waiting out the write, and two writes never overlap.
#[cfg(windows)]
pub(crate) fn save_later(work: impl FnOnce() + Send + 'static) {
    let mut saving = lock(&SAVING);
    let previous = saving.take();
    let spawned =
        thread::Builder::new()
            .name("disktree-save".into())
            .spawn(move || {
                if let Some(previous) = previous {
                    let _ = previous.join();
                }
                // On this thread alone: nobody waits on it, and the parallel
                // steps in making and writing a tree only woke the whole
                // global pool to spin, which cost a cold scan up to a second
                // of CPU.
                match rayon::ThreadPoolBuilder::new().num_threads(1).build() {
                    Ok(alone) => alone.install(work),
                    Err(_) => work(),
                }
            });
    if let Ok(handle) = spawned {
        *saving = Some(handle);
    }
}

/// Walk `root` and return the finished tree. Blocking.
pub fn scan(root: &Path, options: ScanOptions) -> io::Result<Arc<Tree>> {
    let progress = Arc::new(ScanProgress::default());
    let context =
        Arc::new(WalkContext::new(None, options, Arc::clone(&progress)));
    let tree = scan_blocking(root, &context)?;
    progress.finish();
    Ok(tree)
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
    /// The tree being built.
    build: Builder,
    /// Files charged already, when a hardlinked file is charged once. Set
    /// once the walk knows what volume it is on.
    seen: OnceLock<Seen>,
    /// Serial number of the root's volume, on Windows, for a walk that
    /// cannot leave it: every listing takes it instead of asking. See
    /// [`crate::windows::walk_volume`].
    volume: OnceLock<u64>,
}

impl WalkContext {
    fn new(
        known: Option<Known>,
        options: ScanOptions,
        progress: Arc<ScanProgress>,
    ) -> Self {
        Self {
            known,
            seen: OnceLock::new(),
            options,
            progress,
            root_device: Mutex::new(None),
            foreign_mounts: OnceLock::new(),
            never_scanned: OnceLock::new(),
            visited_dirs: Mutex::new(FxHashSet::default()),
            build: Builder::new(WALK_POOL.current_num_threads()),
            volume: OnceLock::new(),
        }
    }
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

    /// Decide what to do with an entry listed in `dir`.
    ///
    /// The entry's path is built only where it is used: most entries are
    /// files, and a disk has millions of them. The name stays in the
    /// entry, which the caller copies it from.
    fn classify(&self, dir: &Path, entry: &impl Listed) -> Classified {
        let listing = match entry.listing() {
            Ok(listing) => listing,
            Err(error) => {
                self.progress.record_error(&entry.entry_path(dir), &error);
                return Classified::Unreadable;
            }
        };

        if !self.options.include_hidden
            && (entry.name().starts_with('.') || entry.hidden())
        {
            return Classified::Skipped;
        }

        let kind = match listing {
            Listing::Symlink => {
                let path = entry.entry_path(dir);
                return self.classify_symlink(&path);
            }
            Listing::Directory => {
                let path = entry.entry_path(dir);
                return self.classify_dir(entry, path);
            }
            Listing::Leaf(kind) => kind,
        };
        match entry.facts(self.options.apparent_size) {
            Ok(facts) => self.leaf(kind, &facts),
            Err(error) => {
                self.progress.record_error(&entry.entry_path(dir), &error);
                Classified::Unreadable
            }
        }
    }

    fn classify_dir(&self, entry: &impl Listed, path: PathBuf) -> Classified {
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
        if self.known.as_ref().is_some_and(|known| known.path == path) {
            return Classified::Known;
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
                    return Classified::Unreadable;
                }
                _ => {}
            }
        }
        Classified::Subdirectory {
            path,
            inode: entry.identity(),
        }
    }

    fn classify_symlink(&self, path: &Path) -> Classified {
        if !self.options.follow_links {
            // Not followed: the link occupies only its target string, which
            // `du` reports as a handful of bytes or nothing at all.
            return match fs::symlink_metadata(path) {
                Ok(meta) => {
                    let facts = Facts::of(&meta, self.options.apparent_size);
                    Classified::Entry(Leaf::of(
                        NodeKind::Symlink,
                        &facts,
                        facts.shared,
                    ))
                }
                Err(error) => {
                    self.progress.record_error(path, &error);
                    Classified::Unreadable
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
            return Classified::Subdirectory {
                path: path.to_path_buf(),
                inode: identity_of(path, &meta),
            };
        }

        let mut facts = Facts::of(&meta, self.options.apparent_size);
        // What the link leads to, so a file that is also reached directly is
        // charged once.
        facts.identity = identity_of(path, &meta);
        self.leaf(kind_of(&meta, meta.file_type()), &facts)
    }

    const fn leaf(&self, kind: NodeKind, facts: &Facts) -> Classified {
        // A followed link reaches a file a second way, whatever its link
        // count says.
        let track = self.options.follow_links || facts.shared;
        Classified::Entry(Leaf::of(kind, facts, track))
    }
}

/// What the walk reads about one directory entry.
///
/// `std::fs::DirEntry` everywhere but Windows. There the standard listing
/// has neither the allocated size nor a file id, so measuring like `du`
/// would cost an open per file; [`crate::windows`] lists a directory with
/// both instead.
trait Listed {
    fn identity(&self) -> Option<(u64, u64)> {
        None
    }

    /// The entry's path inside `dir`, the directory it was listed from.
    fn entry_path(&self, dir: &Path) -> PathBuf;
    /// Non-UTF-8 names are lossy for display. The scan still measures them
    /// correctly; only the reported name is approximate.
    fn name(&self) -> &str;
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

/// A standard directory entry with its name decoded once, up front.
#[cfg(not(windows))]
struct Named {
    entry: DirEntry,
    name: Box<str>,
}

#[cfg(not(windows))]
impl Listed for Named {
    fn entry_path(&self, _dir: &Path) -> PathBuf {
        self.entry.path()
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn listing(&self) -> io::Result<Listing> {
        let file_type = self.entry.file_type()?;
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
        self.entry
            .metadata()
            .map(|meta| Facts::of(&meta, apparent_size))
    }

    fn directory(&self) -> io::Result<Directory> {
        self.entry.metadata().map(|meta| Directory {
            device: device_of(&meta),
            evicted: is_dataless(&meta),
        })
    }
}

#[cfg(windows)]
impl Listed for crate::windows::Entry {
    fn identity(&self) -> Option<(u64, u64)> {
        self.identity()
    }

    fn entry_path(&self, dir: &Path) -> PathBuf {
        self.path(dir)
    }

    fn name(&self) -> &str {
        self.name()
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
fn list(
    path: &Path,
    _volume: Option<u64>,
) -> io::Result<impl Iterator<Item = io::Result<Named>>> {
    Ok(fs::read_dir(path)?.map(|entry| {
        entry.map(|entry| Named {
            name: entry.file_name().into_string().map_or_else(
                |raw| raw.to_string_lossy().into(),
                String::into_boxed_str,
            ),
            entry,
        })
    }))
}

#[cfg(windows)]
fn list(
    path: &Path,
    volume: Option<u64>,
) -> io::Result<crate::windows::ReadDir> {
    crate::windows::read_dir(path, volume)
}

/// What a directory entry turned out to be.
enum Classified {
    /// Descend into this directory on a new task.
    Subdirectory {
        path: PathBuf,
        inode: Option<(u64, u64)>,
    },
    /// A leaf that contributes size.
    Entry(Leaf),
    /// The subtree a narrower scan measured: see [`Known`].
    Known,
    /// Intentionally left out by the scan policy.
    Skipped,
    /// Missing facts must be retried even without another journal record.
    Unreadable,
}

/// A file, link or other leaf as the walk found it. Its name stays in the
/// listing until it is copied into the tree.
struct Leaf {
    kind: NodeKind,
    size: u64,
    modified: i64,
    identity: Option<(u64, u64)>,
}

impl Leaf {
    /// Its identity is kept only when `track` says the walk could meet the
    /// same file again: a hardlink de-duplication set of every file on a
    /// disk is millions of entries, nearly all for files with one name.
    const fn of(kind: NodeKind, facts: &Facts, track: bool) -> Self {
        Self {
            kind,
            size: facts.size,
            modified: facts.modified,
            identity: if track { facts.identity } else { None },
        }
    }

    /// Its entry, its identity's device numbered by `volume`; one the tree
    /// cannot number keeps no identity.
    fn item(&self, volume: impl FnOnce(u64) -> Option<u16>) -> Item {
        let mut item = Item {
            kind: self.kind.code(),
            value: self.size,
            modified: seconds(self.modified),
            ..Item::default()
        };
        if let Some((device, id)) = self.identity
            && let Some(volume) = volume(device)
        {
            item.id = id;
            item.volume = volume;
            item.flags = IDENTIFIED;
        }
        item
    }
}

/// Add `item`, named `name`, to a run whose names are in `text`.
fn push_named(
    run: &mut Vec<Item>,
    text: &mut String,
    mut item: Item,
    name: &str,
) {
    item.at = text.len() as u32;
    item.len = u16::try_from(name.len()).unwrap_or(u16::MAX);
    text.push_str(name.get(..usize::from(item.len)).unwrap_or(name));
    run.push(item);
}

/// One directory being walked, plus the counter that decides when it is done.
#[derive(Debug)]
struct PendingDir {
    path: PathBuf,
    inode: Option<(u64, u64)>,
    parent: Option<Arc<Self>>,
    /// Its place among the tree's directories.
    index: u32,
    /// Starts at 1 for the directory itself; one more per subdirectory task.
    /// When it reaches zero the directory is complete.
    pending: AtomicUsize,
    /// Its entries so far, moved out whole once the directory is complete.
    partial: Mutex<Partial>,
    read_error: AtomicBool,
    depth: usize,
}

/// What a directory is made of before it is finished: the entries its
/// listing found, and its subdirectories' totals as each one finishes.
#[derive(Debug, Default)]
struct Partial {
    /// A subdirectory's entry has its index as its `value`.
    run: Vec<Item>,
    /// The entries' names.
    text: String,
    /// Everything in it, all levels down.
    totals: Totals,
    /// What each subdirectory weighs in the order, by its index.
    keys: Vec<(u32, u64)>,
}

impl PendingDir {
    const fn new(
        path: PathBuf,
        parent: Option<Arc<Self>>,
        index: u32,
        depth: usize,
        inode: Option<(u64, u64)>,
    ) -> Self {
        Self {
            path,
            inode,
            parent,
            index,
            pending: AtomicUsize::new(1),
            partial: Mutex::new(Partial {
                run: Vec::new(),
                text: String::new(),
                totals: Totals::DIRECTORY,
                keys: Vec::new(),
            }),
            read_error: AtomicBool::new(false),
            depth,
        }
    }

    /// Order a completed directory's entries and hand them to the tree;
    /// returns its totals and what it weighs in its parent's order. Only
    /// called when `pending` has reached zero, so every subdirectory has
    /// reported to `self.partial`.
    fn finish(&self, context: &WalkContext) -> (Totals, u64) {
        let Partial {
            mut run,
            text,
            totals,
            mut keys,
        } = std::mem::take(&mut *lock(&self.partial));
        let metric = context.options.metric;
        keys.sort_unstable();
        let key = |item: &Item| {
            if item.is_dir() {
                keys.binary_search_by_key(&(item.value as u32), |&(at, _)| at)
                    .map_or(0, |at| keys[at].1)
            } else {
                item.key(metric)
            }
        };
        // Unstable: names in one directory are distinct, and ties only
        // come from names that decoded to the same lossy text.
        run.sort_unstable_by(|left, right| {
            order(
                (key(left), name_in(&text, left).as_bytes()),
                (key(right), name_in(&text, right).as_bytes()),
            )
        });
        let mut dir = Dir {
            parent: self.parent.as_ref().map_or(NONE, |parent| parent.index),
            flags: if self.read_error.load(Ordering::Relaxed) {
                READ_ERROR
            } else {
                0
            },
            ..Dir::EMPTY
        };
        dir.set_totals(totals);
        if let Some((device, id)) = self.inode
            && let Some(volume) = context.build.volume(device)
        {
            dir.id = id;
            dir.volume = volume;
            dir.flags |= IDENTIFIED;
        }
        let placed = context.build.place(
            self.index,
            dir,
            run.iter().map(|item| (*item, name_in(&text, item))),
            text.len(),
        );
        if placed.is_none() {
            // More entries or names than one run can hold, which no real
            // directory has: shown unreadable, what it holds unknown.
            context.progress.record_error(
                &self.path,
                &io::Error::other("more entries than a tree can hold"),
            );
            let empty = Dir {
                parent: dir.parent,
                id: dir.id,
                volume: dir.volume,
                flags: dir.flags | READ_ERROR,
                ..Dir::EMPTY
            };
            context
                .build
                .place(self.index, empty, std::iter::empty(), 0);
            return (Totals::DIRECTORY, 0);
        }
        (totals, dir.key(metric))
    }
}

fn scan_blocking(
    root: &Path,
    context: &Arc<WalkContext>,
) -> io::Result<Arc<Tree>> {
    let root_meta = fs::metadata(root)?;
    if !root_meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", root.display()),
        ));
    }
    let resolved = root.canonicalize().ok();
    let canonical = resolved.as_deref().unwrap_or(root);
    let mut never = crate::space::never_scanned(root, canonical);
    // Hidden-entry and depth filters can leave an alias as the only route
    // to files. Keep every mount view when either filter is active.
    if context.options.include_hidden && context.options.max_depth.is_none() {
        never.extend(crate::space::repeated_mounts_for(
            root,
            canonical,
            context.options.one_filesystem,
        ));
    }
    let _ = context.never_scanned.set(never.into_iter().collect());
    if context.options.one_filesystem {
        *lock(&context.root_device) = Some(device_of(&root_meta));
        if let Some(foreign) = crate::space::foreign_mounts_for(canonical) {
            let _ = context.foreign_mounts.set(foreign.into_iter().collect());
        }
    }
    // Reading the file table is far faster than any walk, where the
    // volume allows it. On the walk's pool too: see `WALK_POOL`.
    #[cfg(windows)]
    {
        let progress = &context.progress;
        let read = WALK_POOL.install(|| {
            crate::mft::scan(root, canonical, &context.options, progress)
        });
        if let Some(tree) = read {
            let tree = tree?;
            // Cancelled after the reader's last check: the rescan that
            // asked is waiting, and this tree is not wanted.
            if progress.is_cancelled() {
                return Err(crate::mft::cancelled());
            }
            // The reader finishes the tree, kinds too: kept between scans,
            // they are decided again only where a change can reach. It
            // counted every file on the volume; the tree may hold fewer.
            let top = tree.root();
            progress.settle(top.files(), top.dirs(), top.bytes());
            return Ok(tree);
        }
        // The reader gave up, or never started: the walk counts from nothing.
        progress.settle(0, 0, 0);
        // Asking once instead of per directory; see `walk_volume` for
        // when a walk that does not follow links cannot leave the volume.
        // Only on a resolved root: a mapped share's letter kept as typed
        // looks like a local drive.
        if !context.options.follow_links
            && let Some(resolved) = &resolved
            && let Some(serial) = crate::windows::walk_volume(resolved)
        {
            let _ = context.volume.set(serial);
        }
    }
    if context.options.follow_links
        && let Some(key) = identity_of(root, &root_meta)
    {
        lock(&context.visited_dirs).insert(key);
    }

    #[cfg(windows)]
    let cache = walk_cache::Checkpoint::open(canonical, context);
    #[cfg(windows)]
    if let Some(cache) = &cache
        && let Some(mut tree) = WALK_POOL.install(|| cache.resume(context))
    {
        tree.rename(file_name(root));
        let top = tree.root();
        context
            .progress
            .settle(top.files(), top.dirs(), top.bytes());
        return Ok(Arc::new(tree));
    }
    if context.options.dedup_hardlinks {
        // A walk that cannot leave its NTFS volume, grafting nothing kept
        // from an earlier scan, meets only current file ids.
        #[cfg(windows)]
        let current = context.known.is_none()
            && context.volume.get().is_some()
            && crate::windows::on_ntfs(canonical);
        #[cfg(not(windows))]
        let current = false;
        let _ = context.seen.set(if current {
            Seen::by_record()
        } else {
            Seen::new()
        });
    }
    let index = context
        .build
        .reserve(1)
        .ok_or_else(|| io::Error::other("the tree was built already"))?;
    let root_dir = Arc::new(PendingDir::new(
        root.to_path_buf(),
        None,
        index,
        0,
        identity_of(root, &root_meta),
    ));
    let context: &WalkContext = context;
    WALK_POOL.install(|| rayon::scope(|scope| walk(scope, &root_dir, context)));

    let mut tree = context.build.finish(file_name(root));
    // On the walk's pool: the UI's own work on the global pool must not
    // queue behind a scan finishing.
    WALK_POOL.install(|| crate::classify::classify(&mut tree));
    let tree = Arc::new(tree);
    #[cfg(windows)]
    if let Some(cache) = cache {
        cache.save(&tree, context);
    }
    Ok(tree)
}

/// The walk's workers, and the file table reader's. Listing directories
/// contends in the file system, and past some number of threads more of
/// them only queue: on Windows a home directory of 3.4 million files
/// walked in 6.5 s on 16 threads and 8.1 s on all 24. That cap was
/// measured on one 24-thread NTFS laptop; another disk or CPU may want
/// another. Elsewhere the default count. A pool of its own on every
/// platform all the same: work the UI hands the global pool, such as
/// re-ranking a tree, must not queue behind a scan's blocking reads.
static WALK_POOL: LazyLock<rayon::ThreadPool> = LazyLock::new(|| {
    let threads = if cfg!(windows) {
        rayon::current_num_threads().min(16)
    } else {
        rayon::current_num_threads()
    };
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|index| format!("disktree-walk-{index}"))
        .build()
        .expect("a thread pool")
});

/// Entries counted before the shared progress counters are touched. Every
/// worker adding to the same counters per entry keeps one cache line
/// bouncing between cores; per directory, or per this many entries in a
/// wide one so the meter still moves, is invisible to a reader.
const TALLY_EVERY: u64 = 1024;

/// What one directory's listing has found so far, for the progress meter.
#[derive(Default)]
struct Tally {
    files: u64,
    dirs: u64,
    bytes: u64,
}

impl Tally {
    fn flush(&mut self, progress: &ScanProgress) {
        progress.add(self.files, self.dirs, self.bytes);
        *self = Self::default();
    }
}

/// A subdirectory a listing found, to walk next.
struct Subdir {
    path: PathBuf,
    inode: Option<(u64, u64)>,
    /// Where its entry is in the listing's run.
    at: usize,
}

/// Read one directory, spawn a task per subdirectory, then report completion.
///
/// Flat tasks inside the scope: the worker stack never grows with tree depth.
fn walk<'scope>(
    scope: &Scope<'scope>,
    dir: &Arc<PendingDir>,
    context: &'scope WalkContext,
) {
    let mut run: Vec<Item> = Vec::new();
    let mut text = String::new();
    let mut totals = Totals::DIRECTORY;
    let mut keys = Vec::new();
    // Each subdirectory to walk, and where its entry is in `run`.
    let mut subdirs: Vec<Subdir> = Vec::new();
    let mut tally = Tally::default();
    // A depth-limited scan still measures what is directly in the directory,
    // it just does not descend further.
    let descend = context
        .options
        .max_depth
        .is_none_or(|max_depth| dir.depth < max_depth);

    match list(&dir.path, context.volume.get().copied()) {
        Ok(entries) => {
            for entry in entries {
                if context.cancelled() {
                    break;
                }
                match entry {
                    Ok(entry) => {
                        match context.classify(&dir.path, &entry) {
                            Classified::Subdirectory { path, inode } => {
                                tally.dirs += 1;
                                if descend {
                                    subdirs.push(Subdir {
                                        path,
                                        inode,
                                        at: run.len(),
                                    });
                                    let item = Item {
                                        kind: DIRECTORY,
                                        value: u64::from(NONE),
                                        ..Item::default()
                                    };
                                    push_named(
                                        &mut run,
                                        &mut text,
                                        item,
                                        entry.name(),
                                    );
                                }
                            }
                            Classified::Entry(leaf) => {
                                // A link counts as a file on the meter, as
                                // it always has, though the tree does not
                                // count it.
                                tally.files += 1;
                                tally.bytes += leaf.size;
                                let item = context.charge(&leaf);
                                totals.add(item.totals());
                                push_named(
                                    &mut run,
                                    &mut text,
                                    item,
                                    entry.name(),
                                );
                            }
                            Classified::Known => {
                                if let Some((index, known)) =
                                    context.graft(dir.index)
                                {
                                    tally.files += known.files;
                                    tally.dirs += u64::from(known.dirs);
                                    tally.bytes += known.bytes;
                                    totals.add(known);
                                    let weight = match context.options.metric {
                                        Metric::Bytes => known.bytes,
                                        Metric::Files => known.files,
                                    };
                                    keys.push((index, weight));
                                    let item = Item {
                                        kind: DIRECTORY,
                                        value: u64::from(index),
                                        ..Item::default()
                                    };
                                    push_named(
                                        &mut run,
                                        &mut text,
                                        item,
                                        entry.name(),
                                    );
                                }
                            }
                            Classified::Skipped => {}
                            Classified::Unreadable => {
                                dir.read_error.store(true, Ordering::Relaxed);
                            }
                        }
                        if tally.files + tally.dirs >= TALLY_EVERY {
                            tally.flush(&context.progress);
                        }
                    }
                    Err(error) => {
                        context.progress.record_error(&dir.path, &error);
                        dir.read_error.store(true, Ordering::Relaxed);
                    }
                }
            }
        }
        Err(error) => {
            context.progress.record_error(&dir.path, &error);
            dir.read_error.store(true, Ordering::Relaxed);
        }
    }
    tally.flush(&context.progress);

    // Numbers for the subdirectories, taken at once so each is its
    // parent's plus one per subdirectory before it.
    let first = if subdirs.is_empty() {
        None
    } else {
        u32::try_from(subdirs.len())
            .ok()
            .and_then(|count| context.build.reserve(count))
    };
    if let Some(first) = first {
        for (index, subdir) in (first..).zip(&subdirs) {
            run[subdir.at].value = u64::from(index);
        }
    }
    *lock(&dir.partial) = Partial {
        run,
        text,
        totals,
        keys,
    };
    // Past the numbers a tree has room for, which no disk reaches, the
    // subdirectories stay empty.
    if let Some(first) = first {
        for (index, Subdir { path, inode, .. }) in (first..).zip(subdirs) {
            if context.cancelled() {
                break;
            }
            let subdir = Arc::new(PendingDir::new(
                path,
                Some(Arc::clone(dir)),
                index,
                dir.depth + 1,
                inode,
            ));
            dir.pending.fetch_add(1, Ordering::AcqRel);
            scope.spawn(move |scope| walk(scope, &subdir, context));
        }
    }

    signal_done(dir, context);
}

/// Report that one task for `dir` is done: either its own scan, or one of its
/// children. The last one to report finishes the directory and bubbles up.
///
/// This is the whole reason for the `+1` sentinel: a directory is only built
/// once its own scan *and* every subdirectory task has finished, no matter
/// which of them lands last.
fn signal_done(dir: &Arc<PendingDir>, context: &WalkContext) {
    if dir.pending.fetch_sub(1, Ordering::AcqRel) != 1 {
        return;
    }
    let (totals, key) = dir.finish(context);
    let Some(parent) = &dir.parent else {
        return;
    };
    {
        let mut partial = lock(&parent.partial);
        partial.totals.add(totals);
        partial.keys.push((dir.index, key));
    }
    signal_done(parent, context);
}

impl WalkContext {
    /// A leaf's entry, charged nothing when hardlinks count once and
    /// another name of the file was listed first. Which name is charged
    /// is whichever a worker reaches first, so two scans of an unchanged
    /// tree can split it differently between folders; the totals are the
    /// same.
    fn charge(&self, leaf: &Leaf) -> Item {
        let mut item = leaf.item(|device| self.build.volume(device));
        // Every name of a file has the file's size, so one that weighs
        // nothing need not be remembered to be charged once.
        if item.value > 0
            && let Some(seen) = self.seen.get()
            && let Some(key) = leaf.identity
            && !seen.insert(key)
        {
            item.value = 0;
        }
        item
    }

    /// Place the subtree [`Known`] holds in the tree being built, beneath
    /// directory `parent`: its index, and what it adds up to. Its files
    /// are charged again against the walk's, so a hardlink between the two
    /// still counts once, and its totals and order follow from that.
    /// `None` for a subtree that is no tree.
    fn graft(&self, parent: u32) -> Option<(u32, Totals)> {
        let known: &Tree = &self.known.as_ref()?.tree;
        let metric = self.options.metric;
        // Its directories a walk from its root meets, parents first: each
        // old index, and where its parent went.
        let mut placed = vec![NONE; known.dirs.len()];
        let mut order_of: Vec<(u32, usize)> = vec![(0, usize::MAX)];
        *placed.first_mut()? = 0;
        let mut at = 0;
        while let Some(&(old, _)) = order_of.get(at) {
            let dir = known.dirs.get(old as usize)?;
            for item in known.run(dir) {
                if item.is_dir() {
                    let slot =
                        placed.get_mut(usize::try_from(item.value).ok()?)?;
                    // A directory met twice: a loop, not a tree.
                    if *slot != NONE {
                        return None;
                    }
                    *slot = order_of.len() as u32;
                    order_of.push((item.value as u32, at));
                }
            }
            at += 1;
        }
        let first = self.build.reserve(u32::try_from(order_of.len()).ok()?)?;
        let mut sums = vec![Totals::default(); order_of.len()];
        // Deepest first, so each directory totals children already done.
        for (at, &(old, up)) in order_of.iter().enumerate().rev() {
            let dir = known.dirs.get(old as usize)?;
            let text = &known.segs.get(dir.seg as usize)?.text;
            let mut total = Totals::DIRECTORY;
            let mut run: Vec<Item> = known
                .run(dir)
                .iter()
                .map(|&item| {
                    let mut item = item;
                    if item.is_dir() {
                        let local = placed[item.value as usize];
                        item.value = u64::from(first + local);
                        total.add(sums[local as usize]);
                        return item;
                    }
                    let device = known.volume(item.volume);
                    if item.identified() {
                        match self.build.volume(device) {
                            Some(volume) => item.volume = volume,
                            None => item.flags &= !IDENTIFIED,
                        }
                    }
                    if item.value > 0
                        && item.identified()
                        && let Some(seen) = self.seen.get()
                        && !seen.insert((device, item.id))
                    {
                        item.value = 0;
                    }
                    total.add(item.totals());
                    item
                })
                .collect();
            let key = |item: &Item| {
                if item.is_dir() {
                    let local = (item.value as u32 - first) as usize;
                    match metric {
                        Metric::Bytes => sums[local].bytes,
                        Metric::Files => sums[local].files,
                    }
                } else {
                    item.key(metric)
                }
            };
            run.sort_by(|left, right| {
                order(
                    (key(left), name_in(text, left).as_bytes()),
                    (key(right), name_in(text, right).as_bytes()),
                )
            });
            let mut placed_dir = Dir {
                parent: if up == usize::MAX {
                    parent
                } else {
                    first + up as u32
                },
                ..*dir
            };
            placed_dir.set_totals(total);
            if placed_dir.identified() {
                match self.build.volume(known.volume(dir.volume)) {
                    Some(volume) => placed_dir.volume = volume,
                    None => placed_dir.flags &= !IDENTIFIED,
                }
            }
            let bytes = run.iter().map(|item| usize::from(item.len)).sum();
            self.build.place(
                first + at as u32,
                placed_dir,
                run.iter().map(|item| (*item, name_in(text, item))),
                bytes,
            )?;
            sums[at] = total;
        }
        Some((first, sums[0]))
    }
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
pub(crate) fn file_name(path: &Path) -> Box<str> {
    path.file_name()
        .map_or_else(
            || path.to_string_lossy().into_owned(),
            |name| name.to_string_lossy().into_owned(),
        )
        .into()
}

/// A poisoned lock means another task panicked; the data is still a valid tree
/// prefix, and refusing to read it would turn one panic into two.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Metric, Node};
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

    fn scan_dir(root: &Path, options: &ScanOptions) -> Arc<Tree> {
        scan(root, options.clone()).expect("scan")
    }

    fn child<'a>(node: Node<'a>, name: &str) -> Node<'a> {
        node.child_named(name).unwrap_or_else(|| {
            panic!("no child named {name} in {:?}", node.name())
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
        assert_eq!(tree.root().bytes(), 3500);
        assert_eq!(tree.root().files(), 3);
        assert_eq!(tree.root().dirs(), 4, "root, a, a/nested, b");
        assert_eq!(child(tree.root(), "a").bytes(), 3000);
        assert_eq!(child(child(tree.root(), "a"), "nested").bytes(), 2000);
        assert_eq!(child(tree.root(), "b").bytes(), 500);
    }

    #[test]
    fn children_are_ranked_largest_first() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        write(root, "small.bin", 10);
        write(root, "large.bin", 1000);
        write(root, "medium.bin", 100);

        let tree = scan_dir(root, &options());
        let names: Vec<&str> = tree.root().children().map(Node::name).collect();
        assert_eq!(names, vec!["large.bin", "medium.bin", "small.bin"]);
    }

    #[test]
    fn a_directory_reports_its_own_bytes_separately() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        write(root, "direct.bin", 700);
        write(root, "sub/deep.bin", 300);

        let tree = scan_dir(root, &options());
        assert_eq!(tree.root().own_bytes(), 700);
        assert_eq!(tree.root().own_files(), 1);
        assert_eq!(tree.root().bytes(), 1000);
        assert_eq!(tree.root().files(), 2);
    }

    #[test]
    fn hardlinks_are_charged_once_by_default() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        let original = write(root, "original.bin", 4096);
        fs::hard_link(&original, root.join("link.bin")).expect("hard link");

        let deduped = scan_dir(root, &options());
        assert_eq!(deduped.root().bytes(), 4096);
        assert_eq!(deduped.root().files(), 2, "both names still exist");

        let counted = scan_dir(
            root,
            &ScanOptions {
                dedup_hardlinks: false,
                ..options()
            },
        );
        assert_eq!(counted.root().bytes(), 8192);
    }

    #[test]
    fn hidden_entries_are_included_by_default_and_excluded_on_request() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        write(root, ".cache/blob.bin", 900);
        write(root, "visible.bin", 100);

        let default = scan_dir(root, &options());
        assert_eq!(default.root().bytes(), 1000);
        assert_eq!(child(default.root(), ".cache").bytes(), 900);

        let without = scan_dir(
            root,
            &ScanOptions {
                include_hidden: false,
                ..options()
            },
        );
        assert_eq!(without.root().bytes(), 100);
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
        assert_eq!(without.root().bytes(), 100);
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
        assert!(tree.root().bytes() < 200, "{}", tree.root().bytes());
        let link = child(tree.root(), "link");
        assert_eq!(link.kind(), NodeKind::Symlink);
        assert!(link.bytes() < 100, "a link holds only its target string");
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
        assert_eq!(tree.root().bytes(), 42);
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
        assert!(tree.root().bytes() >= 100_000, "{}", tree.root().bytes());
        assert_eq!(tree.root().bytes() % 512, 0, "{}", tree.root().bytes());
        let apparent = scan_dir(temp.path(), &options());
        assert_eq!(apparent.root().bytes(), 100_000);
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
        assert_eq!(tree.root().own_bytes(), 10);
        let a = child(tree.root(), "a");
        assert_eq!(
            a.bytes(),
            20,
            "direct files at the cut-off depth still count"
        );
        assert!(
            a.children().all(|child| !child.is_dir()),
            "descending stopped at depth 1"
        );
        assert_eq!(a.files(), 1);
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
        assert_eq!(child(tree.root(), "one").bytes(), 100_000);
        assert_eq!(tree.root().child(0).map_or("", Node::name), "one");

        let by_files = scan_dir(
            root,
            &ScanOptions {
                metric: Metric::Files,
                ..options()
            },
        );
        assert_eq!(by_files.root().child(0).map_or("", Node::name), "many");
        assert_eq!(by_files.root().child(0).map_or(0, Node::files), 5);
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
        let context =
            Arc::new(WalkContext::new(None, options(), Arc::clone(&progress)));
        let tree = scan_blocking(root, &context).expect("scan still succeeds");
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o700));

        assert_eq!(tree.root().bytes(), 11);
        let node = child(tree.root(), "locked");
        if matches!(readable, Ok(false)) {
            assert!(node.read_error());
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
        assert_eq!(tree.root().bytes(), 3072);

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
                tree: Arc::clone(&known),
            }),
        );
        let tree = loop {
            if let Some(outcome) = handle.poll() {
                break outcome.expect("wide scan");
            }
            thread::sleep(std::time::Duration::from_millis(2));
        };
        let reused = tree
            .root()
            .child_named("inner")
            .expect("inner is in the tree");
        assert_eq!(
            reused.files(),
            known.root().files(),
            "b.bin was never read"
        );
        assert_eq!(reused.bytes(), known.root().bytes());
        assert!(
            tree.root().child_named("outside.bin").is_some(),
            "the rest was walked"
        );
        assert_eq!(tree.root().files(), known.root().files() + 1);
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
            crate::size::human_bytes(tree.root().bytes()),
            tree.root().files()
        );
        for child in tree.root().children().take(14) {
            println!(
                "  {:>10}  {}",
                crate::size::human_bytes(child.bytes()),
                child.name()
            );
        }
        let skipped =
            crate::space::foreign_mounts_for(&root).unwrap_or_default();
        println!("left out: {skipped:?}");
        for needle in ["c", "cache", "node_modules", "zzzz"] {
            let started = std::time::Instant::now();
            let found = crate::filter::filter(tree.root(), &[], needle)
                .expect("needle");
            println!(
                "filter {needle:?}: {} matches, {} in {:.1?}",
                found.count,
                crate::size::human_bytes(found.bytes),
                started.elapsed()
            );
        }
    }
}

//! Turning marked paths into deletions, safely.
//!
//! Three things matter here, in this order: never remove something the user did
//! not point at, never descend into a different filesystem, and always be able
//! to say what happened.
//!
//! Two mechanisms are offered:
//!
//! * [`RemovalMode::Permanent`] — `rm -rf` semantics, implemented with the
//!   standard library rather than by shelling out, so no path ever reaches a
//!   shell and no filename can be misread as an option.
//! * [`RemovalMode::Trash`] — move to the desktop trash, using `trash-put`,
//!   then `gio trash`, then a built-in XDG implementation. The backend is
//!   detected once and named in the UI so the user knows what actually happens.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;

/// One path the user asked to remove.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub path: PathBuf,
    /// Bytes measured when the path was marked; used for the projection.
    pub bytes: u64,
    pub is_dir: bool,
    pub hidden: bool,
}

/// How a removal should be carried out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RemovalMode {
    /// Delete now. Not recoverable.
    #[default]
    Permanent,
    /// Move to the desktop trash so the removal can be undone.
    Trash,
}

impl RemovalMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Permanent => "Delete permanently",
            Self::Trash => "Move to trash",
        }
    }

    pub const fn detail(self) -> &'static str {
        match self {
            Self::Permanent => "rm -rf: unrecoverable",
            Self::Trash => "recoverable until the trash is emptied",
        }
    }
}

/// A target that will not be touched, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocked {
    pub path: PathBuf,
    pub reason: String,
}

/// The work a [`RemovalMode`] will actually do.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// Targets to act on, with any target contained in another removed first.
    pub targets: Vec<Target>,
    /// Marked paths covered by a target above; reported, not acted on.
    pub covered: Vec<Target>,
    /// Marked paths that must not be touched.
    pub blocked: Vec<Blocked>,
}

impl Plan {
    pub fn bytes(&self) -> u64 {
        self.targets.iter().map(|target| target.bytes).sum()
    }

    pub const fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }
}

/// Build the plan for `targets`, which must all live under `root`.
///
/// Paths outside `root` are blocked rather than removed: the tree the user was
/// looking at is the only thing they consented to act on.
pub fn plan(targets: &[Target], root: &Path) -> Plan {
    let root = normalize(root);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut plan = Plan::default();
    let mut accepted: Vec<Target> = Vec::new();

    for target in targets {
        let path = normalize(&target.path);
        if let Some(reason) = refuse(&path, &root, home.as_deref()) {
            plan.blocked.push(Blocked {
                path: target.path.clone(),
                reason,
            });
            continue;
        }
        accepted.push(Target {
            path,
            ..target.clone()
        });
    }

    // A path inside another target is removed with it. Keep the outer one and
    // report the inner one so the review screen can explain the nesting.
    accepted.sort_by(|left, right| left.path.cmp(&right.path));
    let mut outer: Vec<Target> = Vec::new();
    for target in accepted {
        if outer
            .iter()
            .any(|candidate| target.path.starts_with(&candidate.path))
        {
            plan.covered.push(target);
        } else {
            outer.push(target);
        }
    }
    plan.targets = outer;
    plan
}

/// Why this path must not be removed, if it must not.
/// Trees the operating system owns. A whole-disk scan shows them, because
/// they are part of what fills the disk, but files there belong to packages
/// and removing them by hand breaks the system; pacman, paccache and
/// `journalctl --vacuum` are the right tools. Refused even where
/// permissions would allow it, and even inside them.
const SYSTEM_TREES: [&str; 14] = [
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib64",
    "/nix/store",
    "/proc",
    "/run",
    "/sbin",
    "/sys",
    "/usr",
    "/var/lib",
    "/efi",
];

/// The system tree `path` is in, if any. The home directory is never
/// system, wherever it lives.
fn system_tree(path: &Path, home: Option<&Path>) -> Option<&'static str> {
    if home.is_some_and(|home| path.starts_with(normalize(home))) {
        return None;
    }
    SYSTEM_TREES
        .iter()
        .find(|tree| path.starts_with(tree))
        .copied()
}

fn refuse(path: &Path, root: &Path, home: Option<&Path>) -> Option<String> {
    if path.parent().is_none() {
        return Some("the filesystem root cannot be removed".into());
    }
    if path == root {
        return Some("the scanned root cannot be removed".into());
    }
    if home.is_some_and(|home| path == normalize(home)) {
        return Some("the home directory cannot be removed".into());
    }
    if !path.starts_with(root) {
        return Some("outside the scanned root".into());
    }
    if let Some(system) = system_tree(path, home) {
        return Some(format!(
            "part of the system under {system}: use the package manager"
        ));
    }
    if is_mount_point(path) {
        return Some(
            "a mount point: removing it would cross onto another filesystem"
                .into(),
        );
    }
    None
}

/// Whether `path` sits on a different device than its parent, i.e. is a
/// mount point. Descending into one would delete data the user never marked.
#[cfg(unix)]
pub fn is_mount_point(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(meta) = fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.is_dir() {
        return false;
    }
    let Some(parent) = path.parent() else {
        return true;
    };
    match fs::symlink_metadata(parent) {
        Ok(parent_meta) => parent_meta.dev() != meta.dev(),
        // A directory whose parent cannot be stat'ed is not worth the risk.
        Err(_) => true,
    }
}

#[cfg(not(unix))]
pub fn is_mount_point(_path: &Path) -> bool {
    false
}

/// Lexically normalize a path: resolve `.` and `..` without touching the
/// filesystem, so a symlink can never redirect a guard.
///
/// A `..` at the root has nowhere to go, so `/..` is `/` rather than the empty
/// path. A leading `..` on a relative path is preserved.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    out.components().next_back(),
                    Some(Component::Normal(_))
                ) {
                    out.pop();
                } else if !out.has_root() && out.as_os_str().is_empty() {
                    out.push("..");
                }
            }
            Component::Prefix(_)
            | Component::RootDir
            | Component::Normal(_) => {
                out.push(component.as_os_str());
            }
        }
    }
    out
}

/// Which tool, if any, moves files to the desktop trash.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrashBackend {
    /// `trash-put` from trash-cli.
    TrashPut,
    /// `gio trash`, present anywhere `GLib` is installed.
    Gio,
    /// The XDG trash directory, implemented here.
    XdgHome,
    /// macOS's own `/usr/bin/trash` (macOS 14+): the Finder's Trash, with
    /// "Put Back" intact.
    MacOs,
    /// No way to move files to a trash on this machine.
    #[default]
    Unavailable,
}

impl TrashBackend {
    pub const fn is_available(self) -> bool {
        match self {
            Self::TrashPut | Self::Gio | Self::XdgHome | Self::MacOs => true,
            Self::Unavailable => false,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::TrashPut => "trash-put",
            Self::Gio => "gio trash",
            Self::XdgHome => "XDG trash",
            Self::MacOs => "macOS Trash",
            Self::Unavailable => "no trash tool found",
        }
    }

    pub const fn detail(self) -> &'static str {
        match self {
            Self::TrashPut => {
                "uses trash-cli, the same trash as your file manager"
            }
            Self::Gio => "uses GLib, the same trash as your file manager",
            Self::XdgHome => {
                "moves into ~/.local/share/Trash on the same volume"
            }
            Self::MacOs => "moves into the Trash; Put Back works from Finder",
            Self::Unavailable => {
                if cfg!(target_os = "macos") {
                    "needs macOS 14 or later; keep deleting permanently"
                } else {
                    "install trash-cli or keep deleting permanently"
                }
            }
        }
    }
}

/// Detect the best available trash backend for this machine.
///
/// On macOS only the system tool counts: a Homebrew `trash-put` or `gio`
/// would move files into an XDG directory the Finder never shows, which
/// looks like the space was freed while it was not.
pub fn detect_trash_backend() -> TrashBackend {
    if cfg!(target_os = "macos") {
        if Path::new(MACOS_TRASH).is_file() {
            TrashBackend::MacOs
        } else {
            TrashBackend::Unavailable
        }
    } else if which("trash-put") {
        TrashBackend::TrashPut
    } else if which("gio") {
        TrashBackend::Gio
    } else if home_trash_dir().is_some() {
        TrashBackend::XdgHome
    } else {
        TrashBackend::Unavailable
    }
}

const MACOS_TRASH: &str = "/usr/bin/trash";

fn which(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(program);
        fs::metadata(candidate).is_ok_and(|meta| meta.is_file())
    })
}

fn home_trash_dir() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME").map_or_else(
        || {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".local/share"))
        },
        |value| Some(PathBuf::from(value)),
    )?;
    Some(data.join("Trash"))
}

/// What a running removal reports.
#[derive(Clone, Debug)]
pub enum RemovalEvent {
    Start {
        total: usize,
    },
    Item {
        path: PathBuf,
        bytes: u64,
        outcome: Result<(), String>,
    },
    Done {
        removed: u64,
        bytes: u64,
        failed: usize,
    },
}

/// A removal running on its own thread.
#[derive(Debug)]
pub struct RemovalHandle {
    events: Receiver<RemovalEvent>,
    cancel: Arc<AtomicBool>,
}

impl RemovalHandle {
    /// Take the next event, if one has arrived.
    pub fn poll(&self) -> Option<RemovalEvent> {
        // Empty and Disconnected mean the same thing to the caller: there is
        // nothing more to do this tick.
        self.events.try_recv().ok()
    }

    /// Stop before the next target starts.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Start removing the plan's targets on a worker thread.
pub fn spawn(plan: Plan, mode: RemovalMode) -> RemovalHandle {
    let (sender, events) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    let backend = if mode == RemovalMode::Trash {
        detect_trash_backend()
    } else {
        TrashBackend::Unavailable
    };

    let worker = thread::Builder::new()
        .name("disktree-remove".into())
        .spawn({
            let sender = sender.clone();
            move || run(&plan, mode, backend, &worker_cancel, &sender)
        });
    if let Err(error) = worker {
        let _ = sender.send(RemovalEvent::Item {
            path: PathBuf::new(),
            bytes: 0,
            outcome: Err(format!(
                "could not start the removal worker: {error}"
            )),
        });
    }

    RemovalHandle { events, cancel }
}

fn run(
    plan: &Plan,
    mode: RemovalMode,
    backend: TrashBackend,
    cancel: &AtomicBool,
    sender: &mpsc::Sender<RemovalEvent>,
) {
    let _ = sender.send(RemovalEvent::Start {
        total: plan.targets.len(),
    });
    let mut removed = 0_u64;
    let mut bytes = 0_u64;
    let mut failed = 0_usize;

    for target in &plan.targets {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        let outcome = match mode {
            RemovalMode::Permanent => remove_permanently(&target.path),
            RemovalMode::Trash => move_to_trash(&target.path, backend),
        };
        if outcome.is_ok() {
            removed += 1;
            bytes += target.bytes;
        } else {
            failed += 1;
        }
        let _ = sender.send(RemovalEvent::Item {
            path: target.path.clone(),
            bytes: target.bytes,
            outcome: outcome.map_err(|error| error.to_string()),
        });
    }

    let _ = sender.send(RemovalEvent::Done {
        removed,
        bytes,
        failed,
    });
}

/// `rm -rf` semantics: a symlink is unlinked, never followed.
pub fn remove_permanently(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Move one path to the desktop trash.
pub fn move_to_trash(path: &Path, backend: TrashBackend) -> io::Result<()> {
    match backend {
        TrashBackend::TrashPut => run_tool(Path::new("trash-put"), &[], path),
        TrashBackend::Gio => run_tool(Path::new("gio"), &["trash"], path),
        TrashBackend::XdgHome => trash_via_xdg(path),
        TrashBackend::MacOs => trash_via_macos(Path::new(MACOS_TRASH), path),
        TrashBackend::Unavailable => Err(io::Error::other(
            "no trash tool is installed; use permanent deletion instead",
        )),
    }
}

/// Run one trash tool on one path.
///
/// `--` is passed before the path so that a file named `-rf` is read as a path
/// and never as an option. Splitting this out is also what makes the argument
/// order testable without writing to anybody's real trash.
fn run_tool(program: &Path, arguments: &[&str], path: &Path) -> io::Result<()> {
    let output = Command::new(program)
        .args(arguments)
        .arg("--")
        .arg(path)
        .output()?;
    tool_result(program, &output)
}

/// Apple's `trash` has no `--`: it reads one as a file name and fails after
/// trashing the real path. Only absolute paths are handed to it, so a name
/// that starts with `-` can never be read as an option.
fn trash_via_macos(program: &Path, path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::other(
            "refusing to trash a relative path with the macOS tool",
        ));
    }
    let output = Command::new(program).arg(path).output()?;
    tool_result(program, &output)
}

fn tool_result(
    program: &Path,
    output: &std::process::Output,
) -> io::Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(io::Error::other(format!(
            "{} failed: {}",
            program.display(),
            stderr.trim()
        )))
    }
}

/// The XDG trash layout, implemented directly so the app still has a trash on
/// machines without `trash-cli` or `GLib`.
///
/// Only same-volume paths can be trashed this way: the specification forbids
/// copying a file into a trash directory on another filesystem.
fn trash_via_xdg(path: &Path) -> io::Result<()> {
    let trash = home_trash_dir().ok_or_else(|| {
        io::Error::other("neither XDG_DATA_HOME nor HOME is set")
    })?;
    trash_into(path, &trash)
}

/// Move `path` into the trash directory at `trash`.
#[cfg(unix)]
pub fn trash_into(path: &Path, trash: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let files = trash.join("files");
    let info = trash.join("info");
    fs::create_dir_all(&files)?;
    fs::create_dir_all(&info)?;

    let source = fs::symlink_metadata(path)?;
    let trash_meta = fs::metadata(&files)?;
    if source.dev() != trash_meta.dev() {
        return Err(io::Error::other(
            "on a different filesystem than the trash; use permanent deletion",
        ));
    }

    let name = path
        .file_name()
        .ok_or_else(|| {
            io::Error::other("refusing to trash a path without a name")
        })?
        .to_string_lossy()
        .into_owned();
    let (candidate, trashed_name) = unique_name(&files, &name);
    fs::rename(path, &candidate)?;

    let info_path = info.join(format!("{trashed_name}.trashinfo"));
    let contents = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        percent_encode(&path.to_string_lossy()),
        deletion_date()
    );
    if let Err(error) = fs::write(&info_path, contents) {
        // Put it back rather than leaving an entry the trash UI cannot restore.
        let _ = fs::rename(&candidate, path);
        return Err(error);
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn trash_into(_path: &Path, _trash: &Path) -> io::Result<()> {
    Err(io::Error::other(
        "the XDG trash is only implemented on Unix",
    ))
}

/// `name`, or `name.1`, `name.2`, … until the name is free in `dir`.
fn unique_name(dir: &Path, name: &str) -> (PathBuf, String) {
    let first = dir.join(name);
    if !first.exists() {
        return (first, name.to_string());
    }
    for index in 1..10_000 {
        let candidate = format!("{name}.{index}");
        let path = dir.join(&candidate);
        if !path.exists() {
            return (path, candidate);
        }
    }
    (dir.join(name), name.to_string())
}

/// Percent-encode a path for a `.trashinfo` file: everything outside the
/// unreserved set is escaped, so spaces and newlines survive.
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        let unreserved = byte.is_ascii_alphanumeric()
            || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~');
        if unreserved {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn deletion_date() -> String {
    // The specification wants ISO 8601 in local time; chrono is already in the
    // dependency graph, so use it rather than approximating the offset.
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn target(path: &Path, bytes: u64) -> Target {
        Target {
            path: path.to_path_buf(),
            bytes,
            is_dir: path.is_dir(),
            hidden: false,
        }
    }

    fn tree() -> TempDir {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("a/b")).expect("mkdir");
        fs::write(temp.path().join("a/one.bin"), vec![b'x'; 10])
            .expect("write");
        fs::write(temp.path().join("a/c.bin"), vec![b'x'; 20]).expect("write");
        fs::create_dir_all(temp.path().join("other")).expect("mkdir");
        temp
    }

    #[test]
    fn a_plan_keeps_only_the_outermost_targets() {
        let temp = tree();
        let root = temp.path();
        let inner = target(&root.join("a/b"), 5);
        let outer = target(&root.join("a"), 30);
        let separate = target(&root.join("other"), 0);

        let plan =
            plan(&[inner.clone(), outer.clone(), separate.clone()], root);
        assert_eq!(plan.targets.len(), 2);
        assert!(plan.targets.contains(&outer));
        assert!(plan.targets.contains(&separate));
        assert_eq!(plan.covered.len(), 1);
        assert_eq!(plan.covered[0].path, inner.path);
        assert_eq!(plan.bytes(), 30);
    }

    #[test]
    fn the_root_and_home_are_refused() {
        let temp = tree();
        let root = temp.path();
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut targets = vec![target(Path::new("/"), 0), target(root, 0)];
        if let Some(home) = &home {
            targets.push(target(home, 0));
        }

        let plan = plan(&targets, root);
        assert!(plan.is_empty());
        assert_eq!(plan.blocked.len(), targets.len());
        assert!(
            plan.blocked
                .iter()
                .any(|blocked| blocked.reason.contains("home directory"))
                || home.is_none()
        );
    }

    #[test]
    fn paths_outside_the_root_are_refused() {
        let temp = tree();
        let plan = plan(&[target(Path::new("/etc/passwd"), 1)], temp.path());
        assert!(plan.is_empty());
        assert_eq!(plan.blocked[0].reason, "outside the scanned root");
    }

    #[test]
    fn sibling_prefixes_are_not_treated_as_containment() {
        let temp = tree();
        let root = temp.path();
        fs::create_dir_all(root.join("a-real")).expect("mkdir");
        let plan = plan(
            &[target(&root.join("a"), 1), target(&root.join("a-real"), 1)],
            root,
        );
        assert_eq!(plan.targets.len(), 2, "a-real is not inside a");
        assert!(plan.covered.is_empty());
    }

    #[test]
    fn normalize_resolves_dots_without_the_filesystem() {
        assert_eq!(
            normalize(Path::new("/home/tobi/./src/../src/")),
            PathBuf::from("/home/tobi/src")
        );
        assert_eq!(normalize(Path::new("a/../b")), PathBuf::from("b"));
        assert_eq!(normalize(Path::new("/..")), PathBuf::from("/"));
        assert_eq!(normalize(Path::new("../x")), PathBuf::from("../x"));
    }

    #[test]
    fn mount_points_are_recognized() {
        let temp = tree();
        assert!(!is_mount_point(temp.path()));
        // /proc is a mount point everywhere Linux runs this test.
        if Path::new("/proc/self").exists() {
            assert!(is_mount_point(Path::new("/proc")));
        }
    }

    #[test]
    fn permanent_removal_takes_directories_and_leaves_siblings() {
        let temp = tree();
        let doomed = temp.path().join("a/b");
        remove_permanently(&doomed).expect("remove dir");
        assert!(!doomed.exists());
        assert!(temp.path().join("a/c.bin").exists());
    }

    #[test]
    fn permanent_removal_unlinks_a_symlink_instead_of_following_it() {
        let temp = tree();
        let keep = TempDir::new().expect("tempdir");
        fs::write(keep.path().join("precious.bin"), b"data").expect("write");
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(keep.path(), &link).expect("symlink");

        remove_permanently(&link).expect("remove link");
        assert!(!link.exists());
        assert!(keep.path().join("precious.bin").exists());
    }

    #[test]
    fn a_missing_target_reports_not_found() {
        let temp = tree();
        let error = remove_permanently(&temp.path().join("absent"))
            .expect_err("missing");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn percent_encoding_escapes_what_the_spec_requires() {
        assert_eq!(percent_encode("/home/tobi/a b"), "/home/tobi/a%20b");
        assert_eq!(percent_encode("/a/b-c.d_e~f"), "/a/b-c.d_e~f");
        assert_eq!(percent_encode("/a/néw"), "/a/n%C3%A9w");
        assert_eq!(percent_encode("/a\nb"), "/a%0Ab");
    }

    #[test]
    fn trash_names_avoid_collisions() {
        let temp = TempDir::new().expect("tempdir");
        let (first, first_name) = unique_name(temp.path(), "notes");
        assert_eq!(first_name, "notes");
        fs::write(&first, b"x").expect("write");

        let (second, second_name) = unique_name(temp.path(), "notes");
        assert_eq!(second_name, "notes.1");
        assert_ne!(first, second);
    }

    #[test]
    fn the_xdg_trash_moves_a_file_and_records_where_it_came_from() {
        // A private trash directory keeps the test out of the real trash can.
        let trash = TempDir::new().expect("tempdir");
        let work = TempDir::new().expect("tempdir");
        let file = work.path().join("doomed.bin");
        fs::write(&file, b"payload").expect("write");

        trash_into(&file, trash.path()).expect("trashed");

        assert!(!file.exists());
        let moved = trash.path().join("files/doomed.bin");
        assert!(moved.exists(), "moved into the trash");
        assert_eq!(fs::read(&moved).expect("read"), b"payload");
        let info =
            fs::read_to_string(trash.path().join("info/doomed.bin.trashinfo"))
                .expect("info file");
        assert!(info.starts_with("[Trash Info]\nPath="), "{info}");
        assert!(info.contains("doomed.bin"));
        assert!(info.contains("DeletionDate="));
    }

    #[test]
    fn a_trash_info_path_is_restorable() {
        let trash = TempDir::new().expect("tempdir");
        let work = TempDir::new().expect("tempdir");
        let file = work.path().join("with space.bin");
        fs::write(&file, b"x").expect("write");
        trash_into(&file, trash.path()).expect("trashed");

        let info = fs::read_to_string(
            trash.path().join("info/with space.bin.trashinfo"),
        )
        .expect("info file");
        let recorded = info
            .lines()
            .find_map(|line| line.strip_prefix("Path="))
            .expect("a path");
        assert_eq!(recorded, percent_encode(&file.to_string_lossy()));
        assert!(recorded.ends_with("with%20space.bin"));
    }

    #[test]
    fn a_removal_run_reports_every_item_and_a_total() {
        let temp = tree();
        let root = temp.path();
        let plan = plan(
            &[
                target(&root.join("a/b"), 0),
                target(&root.join("a/c.bin"), 20),
                target(&root.join("absent"), 0),
            ],
            root,
        );
        assert_eq!(plan.targets.len(), 3);

        let handle = spawn(plan, RemovalMode::Permanent);
        let mut events = Vec::new();
        for _ in 0..2000 {
            while let Some(event) = handle.poll() {
                events.push(event);
            }
            if matches!(events.last(), Some(RemovalEvent::Done { .. })) {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }

        assert!(matches!(events[0], RemovalEvent::Start { total: 3 }));
        let items = events
            .iter()
            .filter(|event| matches!(event, RemovalEvent::Item { .. }))
            .count();
        assert_eq!(items, 3);
        match events.last() {
            Some(RemovalEvent::Done {
                removed,
                bytes,
                failed,
            }) => {
                assert_eq!(*removed, 2);
                assert_eq!(*failed, 1);
                assert_eq!(*bytes, 20);
            }
            other => panic!("expected Done, got {other:?}"),
        }
        assert!(!root.join("a/b").exists());
        assert!(root.join("a/one.bin").exists());
    }

    #[test]
    fn a_trash_tool_is_called_with_the_path_after_a_separator() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = TempDir::new().expect("tempdir");
        let recording = temp.path().join("argv");
        let script = temp.path().join("fake-trash-put");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n",
                recording.display()
            ),
        )
        .expect("write the script");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
            .expect("chmod");

        // A path that would be an option if the separator were missing.
        let doomed = temp.path().join("-rf");
        fs::write(&doomed, b"not this time").expect("write");
        run_tool(&script, &[], &doomed).expect("the tool ran");

        let recorded = fs::read_to_string(&recording).expect("the argv");
        let lines: Vec<&str> = recorded.lines().collect();
        assert_eq!(lines, vec!["--", doomed.to_string_lossy().as_ref()]);
        assert!(doomed.exists(), "the real tool would have moved it");
    }

    #[test]
    fn a_failing_trash_tool_reports_its_stderr() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = TempDir::new().expect("tempdir");
        let script = temp.path().join("angry-trash-put");
        fs::write(&script, "#!/bin/sh\necho 'no trash here' >&2\nexit 3\n")
            .expect("write");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
            .expect("chmod");

        let error = run_tool(&script, &[], temp.path()).expect_err("it failed");
        assert!(error.to_string().contains("no trash here"), "{error}");
    }

    #[test]
    fn the_macos_tool_refuses_a_relative_path() {
        let error =
            trash_via_macos(Path::new("/usr/bin/true"), Path::new("-rf"))
                .expect_err("relative");
        assert!(error.to_string().contains("relative"), "{error}");
    }

    #[test]
    fn the_macos_tool_gets_the_path_and_nothing_else() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = TempDir::new().expect("tempdir");
        let log = temp.path().join("argv");
        let script = temp.path().join("fake-trash");
        fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n", log.display()),
        )
        .expect("write");
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755))
            .expect("chmod");

        let target = temp.path().join("-rf");
        trash_via_macos(&script, &target).expect("ran");
        let argv = fs::read_to_string(&log).expect("log");
        assert_eq!(argv, format!("{}\n", target.display()));
    }

    #[test]
    fn detection_prefers_a_tool_this_machine_has() {
        let backend = detect_trash_backend();
        if cfg!(target_os = "macos") {
            assert_eq!(backend, TrashBackend::MacOs);
        } else if which("trash-put") {
            assert_eq!(backend, TrashBackend::TrashPut);
        }
        assert!(backend.is_available());
    }

    #[test]
    fn system_trees_are_refused_in_a_whole_disk_scan() {
        let home = Path::new("/home/tobi");
        assert_eq!(
            system_tree(Path::new("/usr/lib/libfoo.so"), Some(home)),
            Some("/usr")
        );
        assert_eq!(
            system_tree(Path::new("/var/lib/pacman"), Some(home)),
            Some("/var/lib")
        );
        assert_eq!(
            system_tree(Path::new("/var/cache/pacman/pkg"), Some(home)),
            None
        );
        assert_eq!(system_tree(Path::new("/opt/thing"), Some(home)), None);
        assert_eq!(
            system_tree(Path::new("/usrlocal"), Some(home)),
            None,
            "components, not prefixes"
        );
        let odd_home = Path::new("/usr/home/tobi");
        assert_eq!(
            system_tree(Path::new("/usr/home/tobi/.cache"), Some(odd_home)),
            None
        );
        let reason =
            refuse(Path::new("/etc/hosts"), Path::new("/"), Some(home));
        assert!(reason.is_some_and(|reason| reason.contains("/etc")));
    }
}

//! Turning marked paths into deletions, safely.
//!
//! Three things matter here, in this order: never remove something the user did
//! not point at, never descend into a different filesystem, and always be able
//! to say what happened.
//!
//! Two mechanisms are offered:
//!
//! * [`RemovalMode::Permanent`] — `rm -rf --one-file-system` semantics,
//!   implemented here rather than by shelling out, so no path ever reaches a
//!   shell and no filename can be misread as an option.
//! * [`RemovalMode::Trash`] — move to the desktop trash. On macOS that is
//!   always the system Trash, through `NSFileManager`, and on Windows the
//!   Recycle Bin. Elsewhere it is `trash-put`, then `gio trash`, then a
//!   built-in XDG implementation. The backend is detected once and named in
//!   the UI so the user knows what actually happens.

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
    /// The scanned root the targets were judged against.
    pub root: PathBuf,
    /// Bytes the targets free on `root`'s volume; see [`Self::reclaim`].
    reclaim: u64,
    /// Bytes the targets free on other volumes; see [`Self::foreign`].
    foreign: u64,
}

impl Plan {
    pub fn bytes(&self) -> u64 {
        self.targets.iter().map(|target| target.bytes).sum()
    }

    /// Bytes the targets free on the volume `root` is on: the only ones the
    /// free space meter may add to that volume's projection.
    ///
    /// [`Self::bytes`] counts every target, which is what a delete list
    /// wants. A mark on another volume — `-X` crosses filesystems, and a
    /// disk mounted under the root can be marked — gives its space back
    /// there, so projecting it here would lend one disk another's bytes.
    pub const fn reclaim(&self) -> u64 {
        self.reclaim
    }

    /// Bytes the targets free on other volumes, which the meter leaves out.
    pub const fn foreign(&self) -> u64 {
        self.foreign
    }

    /// Bytes whose volume could not be told: no saving is claimed for them
    /// either, since a projection the meter cannot stand behind is worse
    /// than a smaller one.
    pub fn unattributed(&self) -> u64 {
        self.bytes().saturating_sub(self.reclaim + self.foreign)
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
    plan_against(targets, root, &mount_points())
}

/// [`plan`] against a given read of the mount table, for testing.
///
/// One read answers both what may be removed and which volume each target's
/// bytes belong to, so the projection cannot be built from a state of the
/// machine the guards never saw.
fn plan_against(targets: &[Target], root: &Path, mounts: &MountTable) -> Plan {
    let root = normalize(root);
    let mut plan = Plan {
        root: root.clone(),
        ..Plan::default()
    };
    let mut accepted: Vec<Target> = Vec::new();

    if targets.is_empty() {
        return plan;
    }
    // Once per plan, not per target: the review screen plans every frame.
    let home = std::env::home_dir().map(|home| Home::of(&home));
    let real_root = fs::canonicalize(&root).ok();

    for target in targets {
        let path = normalize(&target.path);
        let reason =
            refuse(&path, &root, home.as_ref(), &mounts.points, &mounts.media)
                .or_else(|| {
                    linked(&path, &root, real_root.as_deref(), home.as_ref())
                });
        if let Some(reason) = reason {
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
            .any(|candidate| within(&target.path, &candidate.path))
        {
            plan.covered.push(target);
        } else {
            outer.push(target);
        }
    }
    plan.targets = outer;

    // Which volume each target's bytes come back to. Only what comes back to
    // the scanned volume may be shown against its free space.
    for target in &plan.targets {
        let bytes = target.bytes;
        match crate::space::attribution(
            mounts.mounts.as_deref(),
            &plan.root,
            &target.path,
        ) {
            crate::space::Attribution::Scanned => plan.reclaim += bytes,
            crate::space::Attribution::Other => plan.foreign += bytes,
            crate::space::Attribution::Unknown => {}
        }
    }
    plan
}

/// Trees the operating system owns. A whole-disk scan shows them, because
/// they are part of what fills the disk, but files there belong to packages
/// and removing them by hand breaks the system; pacman, paccache,
/// `journalctl --vacuum` or `brew cleanup` are the right tools. Refused even
/// where permissions would allow it, and even inside them.
///
/// One list for every platform: a macOS tree never exists on Linux, and the
/// other way round, so the extra entries cost nothing. On macOS `/etc`,
/// `/var` and `/tmp` are links into `/private`, which a whole-disk scan
/// reaches by its real name. `/Applications` is left out on purpose: moving
/// an app to the Trash is how macOS uninstalls it, and the apps macOS ships
/// live on the read-only system volume anyway.
#[cfg(not(windows))]
const SYSTEM_TREES: [&str; 24] = [
    "/bin",
    "/boot",
    "/dev",
    "/etc",
    "/lib",
    "/lib64",
    "/lib32",
    "/libx32",
    "/nix/store",
    "/gnu/store",
    "/proc",
    "/run",
    "/sbin",
    "/sys",
    "/usr",
    "/var/lib",
    "/efi",
    // Homebrew on Linux, where it lives outside every user's home.
    "/home/linuxbrew/.linuxbrew",
    // macOS.
    "/System",
    "/Library",
    "/private",
    "/cores",
    "/opt/homebrew",
    "/Network",
];

/// How the guards compare paths: lexically normalized, and where the
/// platform has more than one spelling for a place, in one of them.
///
/// macOS reaches the same directory under two names — `/Users/x` and
/// `/System/Volumes/Data/Users/x` — and its disks ignore case by default, so
/// `/library/caches` is `/Library/Caches`. Windows ignores case too, and
/// `\\?\C:\` is `C:\`. A guard that compared the spelling would be passed by
/// the other one. Only for judging: what is removed is always the path as
/// marked.
fn guard_key(path: &Path) -> PathBuf {
    if cfg!(windows) {
        return windows_key(&normalize(path));
    }
    guard_key_for(path, cfg!(target_os = "macos"))
}

fn guard_key_for(path: &Path, macos: bool) -> PathBuf {
    let path = normalize(path);
    if !macos {
        return path;
    }
    // The Data volume's own root stays as it is: it is under /System.
    let path = match path.strip_prefix(crate::space::MACOS_DATA_VOLUME) {
        Ok(rest) if !rest.as_os_str().is_empty() => Path::new("/").join(rest),
        _ => path,
    };
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

#[cfg(windows)]
fn windows_key(path: &Path) -> PathBuf {
    crate::windows::guard_key(path)
}

#[cfg(not(windows))]
fn windows_key(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// `path` is `base` or inside it, by whole components, as the guards
/// compare them.
fn within(path: &Path, base: &Path) -> bool {
    guard_key(path).starts_with(guard_key(base))
}

/// The system trees with their guard keys, computed once: the review screen
/// plans on every frame, for every mark.
fn system_tree_keys() -> &'static [(String, PathBuf)] {
    static KEYS: std::sync::LazyLock<Vec<(String, PathBuf)>> =
        std::sync::LazyLock::new(|| {
            system_trees()
                .into_iter()
                .map(|tree| {
                    let key = guard_key(&tree);
                    (tree.display().to_string(), key)
                })
                .collect()
        });
    &KEYS
}

#[cfg(not(windows))]
fn system_trees() -> Vec<PathBuf> {
    SYSTEM_TREES.iter().map(PathBuf::from).collect()
}

/// [`SYSTEM_TREES`] on Windows: Windows itself, installed programs and
/// their machine-wide data, and what Windows keeps at the top of its drive
/// for restore points, recovery and booting. Taken from the environment, so
/// a system elsewhere is still recognized, and the usual places on the
/// system drive besides.
#[cfg(windows)]
fn system_trees() -> Vec<PathBuf> {
    let drive = system_drive();
    let mut trees: Vec<PathBuf> = [
        "SystemRoot",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ProgramData",
    ]
    .into_iter()
    .filter_map(std::env::var_os)
    .map(PathBuf::from)
    // An empty or relative value would match too much or nothing at all.
    .filter(|tree| tree.is_absolute())
    .collect();
    trees.extend(
        [
            "Windows",
            "Program Files",
            "Program Files (x86)",
            "ProgramData",
            "System Volume Information",
            "$Recycle.Bin",
            "Recovery",
            "Boot",
            "bootmgr",
            // Windows' memory: turned off in Settings (`powercfg /h off` for
            // hibernation), never deleted by hand.
            "pagefile.sys",
            "hiberfil.sys",
            "swapfile.sys",
        ]
        .map(|name| drive.join(name)),
    );
    trees
}

/// The root of the drive Windows runs from, such as `C:\`.
#[cfg(windows)]
fn system_drive() -> PathBuf {
    let mut drive =
        std::env::var_os("SystemDrive").unwrap_or_else(|| "C:".into());
    // `C:` alone is the current directory on C:, not its root.
    drive.push(r"\");
    PathBuf::from(drive)
}

/// What to use instead, said when a system tree is refused.
#[cfg(not(windows))]
const SYSTEM_TOOLS: &str = "remove it with the tool that installed it";

#[cfg(windows)]
const SYSTEM_TOOLS: &str = "use Settings, or the program's own uninstaller";

/// The system tree the path keyed `key` is in, if any. The home directory is
/// never system, wherever it lives.
fn system_tree(key: &Path, home: Option<&Path>) -> Option<&'static str> {
    if home.is_some_and(|home| key.starts_with(home)) {
        return None;
    }
    system_tree_keys()
        .iter()
        .find(|(_, tree)| key.starts_with(tree))
        .map(|(name, _)| name.as_str())
}

/// A system tree strictly inside the path keyed `key`: removing `/opt` takes
/// `/opt/homebrew` with it, and `/var` takes `/var/lib`.
fn system_tree_below(key: &Path) -> Option<&'static str> {
    system_tree_keys()
        .iter()
        .find(|(_, tree)| tree != key && tree.starts_with(key))
        .map(|(name, _)| name.as_str())
}

/// The home directory, as the guards compare it: by key, and by identity.
#[derive(Debug)]
struct Home {
    key: PathBuf,
    /// The key of the path with every link resolved: `/home` may be a link
    /// to `/data/home`, and a scan of `/data` then shows home under a
    /// spelling [`Self::key`] does not match.
    real: Option<PathBuf>,
    /// Device and inode (volume and file id on Windows), so a spelling
    /// [`guard_key`] cannot fold still matches.
    id: Option<(u64, u64)>,
}

impl Home {
    fn of(path: &Path) -> Self {
        Self {
            key: guard_key(path),
            real: fs::canonicalize(path).ok().map(|real| guard_key(&real)),
            id: identity(path, true),
        }
    }
}

fn refuse(
    path: &Path,
    root: &Path,
    home: Option<&Home>,
    mounts: &[PathBuf],
    media: &[PathBuf],
) -> Option<String> {
    let key = guard_key(path);
    let root_key = guard_key(root);
    let home_key = home.map(|home| home.key.as_path());
    if path.parent().is_none() {
        return Some("the filesystem root cannot be removed".into());
    }
    if key == root_key {
        return Some("the scanned root cannot be removed".into());
    }
    if cfg!(windows)
        && let Some(name) = path.components().find_map(|part| match part {
            Component::Normal(name) => {
                misread_by_win32(&name.to_string_lossy()).then_some(name)
            }
            _ => None,
        })
    {
        return Some(format!(
            "Windows would read {} as a different name, and remove that",
            name.display()
        ));
    }
    if home_key.is_some_and(|home| key == home)
        || home
            .and_then(|home| home.id)
            .is_some_and(|id| identity(path, false) == Some(id))
    {
        return Some("the home directory cannot be removed".into());
    }
    // On Linux `/home` is usually a mount point of its own and refused as
    // one; on macOS `/Users` is on the same volume as `/`, and removing it
    // would empty the home directory before failing on `/Users` itself.
    if home_key.is_some_and(|home| home.starts_with(&key)) {
        return Some("it contains the home directory".into());
    }
    // Elevated through another account's credentials, home is that admin's,
    // and the user's own profile is just another folder; elevation opens
    // every other profile too. So on Windows each directory in the profiles
    // folder (`C:\Users\tobi`, `Public`, `Default`) is refused, taken from
    // Windows and not from home, whose parent could be anything. What is
    // inside a profile stays removable, as under one's own home.
    if profiles_key().is_some_and(|profiles| is_profile(path, &key, profiles)) {
        return Some("a user profile cannot be removed".into());
    }
    // By spelling too: the Data volume's root keeps its name as a key, while
    // everything under it is keyed as under `/`.
    if !key.starts_with(&root_key) && !path.starts_with(root) {
        return Some("outside the scanned root".into());
    }
    if let Some(system) = system_tree(&key, home_key)
        && !(system == "/run"
            && on_media(&key, mounts, media)
            && fs::canonicalize(path).is_ok_and(|real| real == path))
    {
        return Some(format!(
            "part of the system under {system}: {SYSTEM_TOOLS}"
        ));
    }
    if let Some(system) = system_tree_below(&key) {
        return Some(format!(
            "it contains {system}, which is part of the system"
        ));
    }
    if mounts.contains(&key) || is_mount_point(path) {
        return Some(
            "a mount point: removing it would cross onto another filesystem"
                .into(),
        );
    }
    if let Some(mount) = mount_below(&key, mounts) {
        return Some(format!(
            "{} is mounted inside it: removing it would reach into another \
             filesystem",
            mount.display()
        ));
    }
    None
}

/// A name Win32 reads as some other one. It strips trailing dots and
/// spaces, so `C:\Users.` opens `C:\Users`; it reads `NUL` or `COM1` in
/// any directory as a device; and after a colon comes a stream of another
/// file. NTFS keeps such names when they arrive through `\\?\` or WSL,
/// and the scan lists them as stored, but the trash and the removal open
/// them through Win32.
fn misread_by_win32(name: &str) -> bool {
    const DEVICES: [&str; 4] = ["con", "prn", "aux", "nul"];
    if name.ends_with(['.', ' ']) || name.contains(':') {
        return true;
    }
    // `nul.txt` was a device too, before Windows 11: refused either way.
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end()
        .to_lowercase();
    let numbered = ["com", "lpt"].iter().any(|device| {
        stem.strip_prefix(device).is_some_and(|number| {
            matches!(number, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8")
                || matches!(number, "9" | "\u{b9}" | "\u{b2}" | "\u{b3}")
        })
    });
    numbered || DEVICES.contains(&stem.as_str())
}

/// Why `path`, inside `root` as spelled, is somewhere else in fact, if it
/// is. The guards compare spellings, and a link or junction on the way
/// down makes a spelling inside the root name something outside it: the
/// trash, and removal on Windows, follow it. Links above the root are the
/// user's own choice of root, so the real path is compared with the real
/// root. The home directory is looked for again by real path, too.
///
/// Anything that cannot be resolved passes: the removal of a path that is
/// gone fails on its own.
fn linked(
    path: &Path,
    root: &Path,
    real_root: Option<&Path>,
    home: Option<&Home>,
) -> Option<String> {
    let (parent, name) = (path.parent()?, path.file_name()?);
    let real = guard_key(&fs::canonicalize(parent).ok()?.join(name));
    if let (Some(real_root), Ok(rest)) = (real_root, path.strip_prefix(root))
        && real != guard_key(&real_root.join(rest))
    {
        return Some(format!(
            "reached through a link: it is really {}",
            real.display()
        ));
    }
    if home
        .and_then(|home| home.real.as_ref())
        .is_some_and(|home| home.starts_with(&real))
    {
        return Some("it contains the home directory".into());
    }
    None
}

/// `(device, inode)`: what makes two spellings one directory entry.
/// `follow` decides whether a symlink at `path` answers for itself or for
/// what it points to.
#[cfg(unix)]
fn identity(path: &Path, follow: bool) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = if follow {
        fs::metadata(path)
    } else {
        fs::symlink_metadata(path)
    };
    meta.ok().map(|meta| (meta.dev(), meta.ino()))
}

/// `(volume serial, file id)`. Always what a link leads to: a junction to
/// the home directory is refused as the home directory, which is only ever
/// too careful.
#[cfg(windows)]
fn identity(path: &Path, _follow: bool) -> Option<(u64, u64)> {
    crate::windows::identity(path)
}

#[cfg(not(any(unix, windows)))]
const fn identity(_path: &Path, _follow: bool) -> Option<(u64, u64)> {
    None
}

/// How long a read of the mount table is used before it is read again. A
/// disk plugged in is noticed within this, and the removal itself stops at a
/// mount boundary whatever the table said.
const MOUNTS_FRESH: std::time::Duration = std::time::Duration::from_secs(2);

/// Paths and safe media exceptions must come from the same snapshot.
#[derive(Default)]
struct MountTable {
    points: Vec<PathBuf>,
    media: Vec<PathBuf>,
    /// The table itself where the platform keeps one, which tells the
    /// scanned volume from another for the meter's projection. `None` where
    /// there is none to read, and the platform's own device test answers.
    mounts: Option<Vec<crate::space::Mount>>,
}

fn read_mount_table() -> MountTable {
    // The same table the scan reads for its volume, so a projection is
    // judged by the rule the tree itself was built under.
    let mounts = crate::space::mount_table();
    if let Ok(table) = fs::read_to_string("/proc/self/mountinfo") {
        let views = crate::space::parse_mountinfo(&table);
        return MountTable {
            points: views.iter().map(|mount| guard_key(&mount.point)).collect(),
            media: media_roots(&views),
            mounts,
        };
    }
    MountTable {
        points: read_mount_points(),
        media: Vec::new(),
        mounts,
    }
}

/// Only real disks mounted by the desktop under /run/media qualify. A
/// directory with that spelling, a bind of a system tree, and virtual
/// filesystems retain the /run guard. Missing information fails closed.
fn media_roots(mounts: &[crate::space::MountView]) -> Vec<PathBuf> {
    mounts
        .iter()
        .filter(|mount| {
            mount.point.starts_with("/run/media")
                && mount.point != Path::new("/run/media")
                // An overmounted disk can remain in mountinfo. Ambiguous
                // stacked views must never grant a deletion exception.
                && mounts.iter().filter(|other| other.point == mount.point).count() == 1
                && mount.fs_root == Path::new("/")
                && mount.source.starts_with("/dev/")
                && matches!(
                    mount.fstype.as_str(),
                    "ext2"
                        | "ext3"
                        | "ext4"
                        | "btrfs"
                        | "xfs"
                        | "f2fs"
                        | "vfat"
                        | "exfat"
                        | "ntfs"
                        | "ntfs3"
                        | "fuseblk"
                        | "fuse.ntfs-3g"
                )
                && !mounts.iter().any(|other| {
                    !other.point.starts_with("/run/media")
                        && (other.point == Path::new("/")
                            || system_tree(&other.point, None).is_some())
                        && (other.device == mount.device
                            || other.source == mount.source)
                })
        })
        .map(|mount| mount.point.clone())
        .collect()
}

fn on_media(key: &Path, mounts: &[PathBuf], media: &[PathBuf]) -> bool {
    // Nested mounts do not inherit their parent's exception.
    mounts
        .iter()
        .filter(|point| key.starts_with(point))
        .max_by_key(|point| point.components().count())
        .is_some_and(|point| key != point && media.contains(point))
}

/// The last read of the mount table, and whether a read is under way.
struct MountCache {
    read: Option<std::time::Instant>,
    mounts: Option<Arc<MountTable>>,
    refreshing: bool,
}

static MOUNTS: std::sync::Mutex<MountCache> =
    std::sync::Mutex::new(MountCache {
        read: None,
        mounts: None,
        refreshing: false,
    });

/// Every mount point on this machine, as guard keys, as last read.
///
/// Never blocks on the read: the review screen plans on every frame, and on
/// macOS the table comes from running `mount`. A stale table starts a read on
/// its own thread and is used meanwhile; before the first read lands it is
/// empty, which is why the app primes it at start with
/// [`prime_mount_points`].
fn mount_points() -> Arc<MountTable> {
    let mut cache = MOUNTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let fresh = cache.read.is_some_and(|read| read.elapsed() < MOUNTS_FRESH);
    if !fresh && !cache.refreshing {
        cache.refreshing = true;
        let spawned = thread::Builder::new()
            .name("disktree-mounts".into())
            .spawn(|| {
                let mounts = Arc::new(read_mount_table());
                let mut cache = MOUNTS
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                cache.mounts = Some(mounts);
                cache.read = Some(std::time::Instant::now());
                cache.refreshing = false;
            });
        if spawned.is_err() {
            cache.refreshing = false;
        }
    }
    cache.mounts.clone().unwrap_or_default()
}

/// Start reading the mount table, so it is there by the time anything is
/// marked.
pub fn prime_mount_points() {
    let _ = mount_points();
}

/// Read the mount points now: from `/proc/self/mounts`, on macOS from
/// `mount`, which has no `/proc`, and on Windows from the volume list.
/// Empty where none can be read.
fn read_mount_points() -> Vec<PathBuf> {
    let points: Vec<PathBuf> = if cfg!(windows) {
        windows_mount_points()
    } else if let Ok(table) = fs::read_to_string("/proc/self/mounts") {
        crate::space::parse_mounts(&table)
            .into_iter()
            .map(|mount| mount.point)
            .collect()
    } else if cfg!(target_os = "macos") {
        Command::new("/sbin/mount")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| {
                crate::space::parse_macos_mounts(&String::from_utf8_lossy(
                    &output.stdout,
                ))
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    points.iter().map(|point| guard_key(point)).collect()
}

#[cfg(windows)]
fn windows_mount_points() -> Vec<PathBuf> {
    crate::windows::mount_points()
}

#[cfg(not(windows))]
const fn windows_mount_points() -> Vec<PathBuf> {
    Vec::new()
}

/// The key of the folder every user profile is in, `C:\Users` as
/// installed; asked of Windows once, since it does not move while running.
#[cfg(windows)]
fn profiles_key() -> Option<&'static Path> {
    static PROFILES: std::sync::LazyLock<Option<PathBuf>> =
        std::sync::LazyLock::new(|| {
            crate::windows::user_profiles_dir().map(|dir| guard_key(&dir))
        });
    PROFILES.as_deref()
}

#[cfg(not(windows))]
const fn profiles_key() -> Option<&'static Path> {
    None
}

/// Whether `path`, keyed `key`, is a profile: a directory directly in
/// `profiles`. A file or a link there is no profile and stays removable.
fn is_profile(path: &Path, key: &Path, profiles: &Path) -> bool {
    key.parent() == Some(profiles)
        && fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// A mount point strictly inside the path keyed `key`, if there is one. A
/// scan that stays on one filesystem never shows what is mounted there, so
/// the user cannot have meant to remove it.
fn mount_below<'a>(key: &Path, mounts: &'a [PathBuf]) -> Option<&'a Path> {
    mounts
        .iter()
        .find(|mount| mount.as_path() != key && mount.starts_with(key))
        .map(PathBuf::as_path)
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

/// Whether `path` is a folder another volume is mounted on.
#[cfg(windows)]
pub fn is_mount_point(path: &Path) -> bool {
    crate::windows::is_mount_point(path)
}

#[cfg(not(any(unix, windows)))]
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
    /// The macOS Trash, through `NSFileManager`: the same move as Finder's
    /// Move to Trash, into the Trash of the volume the item is on.
    MacOs,
    /// `trash-put` from trash-cli.
    TrashPut,
    /// `gio trash`, present anywhere `GLib` is installed.
    Gio,
    /// The XDG trash directory, implemented here.
    XdgHome,
    /// The Windows Recycle Bin, through the shell's own file operation.
    RecycleBin,
    /// No way to move files to a trash on this machine.
    #[default]
    Unavailable,
}

impl TrashBackend {
    pub const fn is_available(self) -> bool {
        match self {
            Self::MacOs
            | Self::TrashPut
            | Self::Gio
            | Self::XdgHome
            | Self::RecycleBin => true,
            Self::Unavailable => false,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::MacOs => "the Trash",
            Self::TrashPut => "trash-put",
            Self::Gio => "gio trash",
            Self::XdgHome => "XDG trash",
            Self::RecycleBin => "the Recycle Bin",
            Self::Unavailable => "no trash tool found",
        }
    }

    pub const fn detail(self) -> &'static str {
        match self {
            Self::MacOs => "the same Trash as Finder, on the item's own disk",
            Self::TrashPut => {
                "uses trash-cli, the same trash as your file manager"
            }
            Self::Gio => "uses GLib, the same trash as your file manager",
            Self::XdgHome => {
                "moves into ~/.local/share/Trash on the same volume"
            }
            Self::RecycleBin => "the same Recycle Bin as File Explorer",
            Self::Unavailable => {
                "install trash-cli or keep deleting permanently"
            }
        }
    }
}

/// Detect the best available trash backend for this machine. Windows
/// always has its Recycle Bin.
#[cfg(windows)]
pub const fn detect_trash_backend() -> TrashBackend {
    TrashBackend::RecycleBin
}

/// Detect the best available trash backend for this machine.
///
/// On macOS the system Trash is always there, and the Linux tools are not
/// a substitute: Homebrew's `trash-put` writes the XDG layout, which Finder
/// never shows, so their files would look recoverable and not be.
#[cfg(target_os = "macos")]
pub const fn detect_trash_backend() -> TrashBackend {
    TrashBackend::MacOs
}

/// Detect the best available trash backend for this machine.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn detect_trash_backend() -> TrashBackend {
    if which("trash-put") {
        TrashBackend::TrashPut
    } else if which("gio") {
        TrashBackend::Gio
    } else if home_trash_dir().is_some() {
        TrashBackend::XdgHome
    } else {
        TrashBackend::Unavailable
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
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
    // The plan was judged against a mount table up to `MOUNTS_FRESH` old,
    // or, right after start, not read yet. Judged again against a fresh
    // read, here on the worker, before anything is touched.
    let mounts = read_mount_table();
    // Links too: one put on the way down since the plan was made would lead
    // the trash out of the root. Permanent removal on Unix refuses that by
    // itself; see [`open_parent`].
    let home = std::env::home_dir().map(|home| Home::of(&home));
    let real_root = fs::canonicalize(&plan.root).ok();

    for target in &plan.targets {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        // The /run/media exception must still hold when removal starts;
        // a disk may have been unmounted since the review was displayed.
        let reason = target
            .path
            .starts_with("/run/media")
            .then(|| {
                refuse(
                    &target.path,
                    &plan.root,
                    home.as_ref(),
                    &mounts.points,
                    &mounts.media,
                )
            })
            .flatten()
            .or_else(|| mounted(&target.path, &mounts.points))
            .or_else(|| {
                linked(
                    &target.path,
                    &plan.root,
                    real_root.as_deref(),
                    home.as_ref(),
                )
            });
        let outcome = if let Some(reason) = reason {
            Err(io::Error::other(reason))
        } else {
            match mode {
                RemovalMode::Permanent => {
                    remove_permanently(&target.path, &plan.root)
                }
                RemovalMode::Trash => move_to_trash(&target.path, backend),
            }
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

    // NTFS writes the file table's records lazily, and the elevated rescan
    // of a whole drive reads that table from disk, past the cache:
    // unflushed, the removed entries would still show. Only then; a folder
    // is walked. Failing only lets the table lag again.
    #[cfg(windows)]
    if removed > 0
        && plan.root.parent().is_none()
        && crate::access::administrator() == Some(true)
    {
        let _ = crate::windows::flush_volume(&plan.root);
    }

    let _ = sender.send(RemovalEvent::Done {
        removed,
        bytes,
        failed,
    });
}

/// Why `path` would reach into another filesystem, if it would: the
/// mount-point rules of [`refuse`], asked again at removal time.
fn mounted(path: &Path, mounts: &[PathBuf]) -> Option<String> {
    if is_mount_point(path) {
        return Some("it became a mount point since it was marked".into());
    }
    mount_below(&guard_key(path), mounts).map(|mount| {
        format!(
            "{} is mounted inside it: removing it would reach into another \
             filesystem",
            mount.display()
        )
    })
}

/// `rm -rf --one-file-system` semantics: a symlink is unlinked, never
/// followed, and nothing on another filesystem is touched.
///
/// `fs::remove_dir_all` would descend into anything mounted inside `path` and
/// empty it before failing on the mount point itself, so the walk is done here
/// instead: every directory is opened relative to its parent with
/// `O_NOFOLLOW`, and one on a different device or mount stops the removal.
/// (From tobi/disktree#10.)
///
/// The same goes for the way down to `path` from `root`, the scanned root:
/// see [`open_parent`].
#[cfg(unix)]
pub fn remove_permanently(path: &Path, root: &Path) -> io::Result<()> {
    use rustix::fs::{AtFlags, FileType, statat, unlinkat};

    let (parent, name) = open_parent(path, root)?;
    let stat = statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        unlinkat(&parent, name, AtFlags::empty())?;
        return Ok(());
    }
    let dir = open_dir(&parent, name)?;
    let top = Volume::of(&dir)?;
    remove_contents(&dir, &top, path)?;
    drop(dir);
    unlinkat(&parent, name, AtFlags::REMOVEDIR)?;
    Ok(())
}

/// The directory holding `path`, and `path`'s own name in it.
///
/// Opened one component at a time from `root`, none of them followed if it
/// is a symlink. The scan does not follow links, so no marked path has one
/// between the root and itself; one that does now was put there after the
/// scan, and following it would remove whatever it points to — a directory
/// swapped for a link to `~` would take `~/x` in place of the `/tmp/d/x`
/// that was marked. `rm -rf` has the same race; this closes it. Links above
/// the root are followed: they are how the user named it, and on macOS
/// `/tmp` and `/var` are links themselves.
#[cfg(unix)]
fn open_parent<'a>(
    path: &'a Path,
    root: &Path,
) -> io::Result<(std::os::fd::OwnedFd, &'a std::ffi::OsStr)> {
    use rustix::io::Errno;

    let not_marked = || {
        io::Error::other(format!(
            "{} is not an absolute, normalized path",
            path.display()
        ))
    };
    let name = path.file_name().ok_or_else(not_marked)?;
    let below = path
        .parent()
        .and_then(|parent| parent.strip_prefix(root).ok())
        .ok_or_else(|| {
            io::Error::other(format!(
                "{} is not inside the scanned root {}",
                path.display(),
                root.display()
            ))
        })?;
    let mut dir = open_root(root)?;
    for component in below.components() {
        match component {
            Component::Normal(part) => {
                dir = open_step(&dir, part).map_err(|error| match error {
                    Errno::LOOP | Errno::NOTDIR => io::Error::other(format!(
                        "{} changed since the scan: {} is no longer a \
                         directory, so nothing was removed",
                        path.display(),
                        part.display()
                    )),
                    error => error.into(),
                })?;
            }
            _ => return Err(not_marked()),
        }
    }
    Ok((dir, name))
}

/// Open the scanned root, following links: see [`open_parent`].
#[cfg(unix)]
fn open_root(root: &Path) -> rustix::io::Result<std::os::fd::OwnedFd> {
    use rustix::fs::{Mode, OFlags, open};
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let access = OFlags::PATH;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let access = OFlags::RDONLY;
    open(
        root,
        access | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// Open a directory on the way down to a target, not following a symlink.
/// Only a handle to walk through: on Linux `O_PATH`, which needs no read
/// permission, just as a path lookup does not.
#[cfg(unix)]
fn open_step<P: rustix::path::Arg>(
    parent: impl std::os::fd::AsFd,
    name: P,
) -> rustix::io::Result<std::os::fd::OwnedFd> {
    use rustix::fs::{Mode, OFlags, openat};
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let access = OFlags::PATH;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let access = OFlags::RDONLY;
    openat(
        parent,
        name,
        access | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

#[cfg(not(unix))]
pub fn remove_permanently(path: &Path, _root: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else if is_dir_link(&meta) {
        // Windows removes a link to a directory, a junction or a directory
        // symlink, as a directory: `RemoveDirectoryW` takes the link and
        // leaves its target alone.
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    }
}

/// Where a directory lives, as far as crossing into another filesystem goes.
#[cfg(unix)]
struct Volume {
    device: rustix::fs::Stat,
    /// The mount the directory belongs to, where the kernel reports it (Linux,
    /// `statx`). See [`Self::contains`].
    mount: Option<u64>,
}

#[cfg(unix)]
impl Volume {
    fn of(dir: &std::os::fd::OwnedFd) -> io::Result<Self> {
        Ok(Self {
            device: rustix::fs::fstat(dir)?,
            mount: mount_id(dir),
        })
    }

    /// Whether `other` is on the same mount. Where the kernel names mounts,
    /// that decides: a btrfs subvolume that is not mounted has its own
    /// `st_dev` but is the same mount, and the scan showed it as part of the
    /// volume, so it goes with the directory it is in; a bind mount keeps
    /// `st_dev` but is a mount of its own, and stops the removal. Without
    /// mount IDs (macOS) the device decides.
    const fn contains(&self, other: &Self) -> bool {
        match (self.mount, other.mount) {
            (Some(one), Some(two)) => one == two,
            _ => self.device.st_dev == other.device.st_dev,
        }
    }
}

#[cfg(target_os = "linux")]
fn mount_id(dir: &std::os::fd::OwnedFd) -> Option<u64> {
    use rustix::fs::{AtFlags, StatxFlags, statx};
    statx(dir, c"", AtFlags::EMPTY_PATH, StatxFlags::MNT_ID)
        .ok()
        .filter(|stat| stat.stx_mask & StatxFlags::MNT_ID.bits() != 0)
        .map(|stat| stat.stx_mnt_id)
}

#[cfg(all(unix, not(target_os = "linux")))]
const fn mount_id(_dir: &std::os::fd::OwnedFd) -> Option<u64> {
    None
}

#[cfg(unix)]
fn open_dir<P: rustix::path::Arg>(
    parent: impl std::os::fd::AsFd,
    name: P,
) -> rustix::io::Result<std::os::fd::OwnedFd> {
    use rustix::fs::{Mode, OFlags, openat};
    openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
}

/// Empty the directory open at `dir`, staying on the volume of `top`.
#[cfg(unix)]
fn remove_contents(
    dir: &std::os::fd::OwnedFd,
    top: &Volume,
    path: &Path,
) -> io::Result<()> {
    use rustix::fs::{AtFlags, Dir, FileType, unlinkat};
    use rustix::io::Errno;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt as _;

    // Names first, then removal: unlinking while a directory stream is open
    // on the same directory may skip entries.
    let mut entries = Vec::new();
    for entry in Dir::read_from(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name != c"." && name != c".." {
            entries.push((name.to_owned(), entry.file_type()));
        }
    }

    for (name, kind) in entries {
        if !matches!(kind, FileType::Directory | FileType::Unknown) {
            unlinkat(dir, &name, AtFlags::empty())?;
            continue;
        }
        let child = match open_dir(dir, &name) {
            Ok(child) => child,
            // Not a directory after all: a symlink, or a file on a
            // filesystem that does not report types.
            Err(Errno::NOTDIR | Errno::LOOP) => {
                unlinkat(dir, &name, AtFlags::empty())?;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        // Only directories need their path, for the message below.
        let child_path = path.join(OsStr::from_bytes(name.to_bytes()));
        if !top.contains(&Volume::of(&child)?) {
            return Err(io::Error::other(format!(
                "stopped at {}: another filesystem is mounted there, and \
                 nothing on it was touched",
                child_path.display()
            )));
        }
        remove_contents(&child, top, &child_path)?;
        drop(child);
        unlinkat(dir, &name, AtFlags::REMOVEDIR)?;
    }
    Ok(())
}

#[cfg(windows)]
fn is_dir_link(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::FileTypeExt as _;
    meta.file_type().is_symlink_dir()
}

#[cfg(not(any(unix, windows)))]
const fn is_dir_link(_meta: &fs::Metadata) -> bool {
    false
}

/// Move one path to the desktop trash.
pub fn move_to_trash(path: &Path, backend: TrashBackend) -> io::Result<()> {
    match backend {
        TrashBackend::TrashPut => run_tool(Path::new("trash-put"), &[], path),
        TrashBackend::Gio => run_tool(Path::new("gio"), &["trash"], path),
        TrashBackend::XdgHome => trash_via_xdg(path),
        TrashBackend::MacOs => trash_via_macos(path),
        TrashBackend::RecycleBin => recycle(path),
        TrashBackend::Unavailable => Err(io::Error::other(
            "no trash tool is installed; use permanent deletion instead",
        )),
    }
}

/// Move one path to the macOS Trash.
///
/// The `trash` crate resolves the parent directory and keeps the final name,
/// so a symlink is trashed itself and its target is left alone. A name that is
/// not UTF-8 is refused rather than handed over: the crate would
/// percent-encode it into a different path.
#[cfg(target_os = "macos")]
fn trash_via_macos(path: &Path) -> io::Result<()> {
    use trash::macos::{DeleteMethod, TrashContextExtMacos as _};

    if path.to_str().is_none() {
        return Err(io::Error::other(
            "the name is not UTF-8, which the Trash cannot take; \
             use permanent deletion",
        ));
    }
    let mut context = trash::TrashContext::default();
    // Finder's method asks for permission to control Finder and plays a
    // sound per call; NSFileManager does neither.
    context.set_delete_method(DeleteMethod::NsFileManager);
    context.delete(path).map_err(|error| {
        io::Error::other(format!(
            "could not move to the Trash ({error}); network and read-only \
             disks have none, so use permanent deletion there"
        ))
    })
}

#[cfg(not(target_os = "macos"))]
fn trash_via_macos(_path: &Path) -> io::Result<()> {
    Err(io::Error::other("the macOS Trash only exists on macOS"))
}

/// Hand `path` to the Recycle Bin. `trash` asks the shell to warn before it
/// destroys anything rather than recycling it (`FOF_WANTNUKEWARNING`), so
/// the reversible path cannot turn into a permanent one silently.
#[cfg(windows)]
fn recycle(path: &Path) -> io::Result<()> {
    trash::delete(path).map_err(io::Error::other)
}

#[cfg(not(windows))]
fn recycle(_path: &Path) -> io::Result<()> {
    Err(io::Error::other("the Recycle Bin is only on Windows"))
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
#[cfg(unix)]
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

#[cfg(unix)]
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

    /// [`system_tree`] for plain paths, keyed the way [`refuse`] keys them.
    #[cfg(not(windows))]
    fn tree_of(path: &Path, home: &Path) -> Option<&'static str> {
        system_tree(&guard_key(path), Some(&guard_key(home)))
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

    #[cfg(target_os = "linux")]
    #[test]
    fn desktop_media_exception_requires_an_independent_disk() {
        let views = crate::space::parse_mountinfo(
            "1 0 8:1 / / rw - ext4 /dev/root rw\n\
             2 1 0:20 / /run rw - tmpfs tmpfs rw\n\
             3 2 8:2 / /run/media/user/Games rw - ext4 /dev/sdb rw\n\
             4 2 8:1 / /run/media/user/System rw - ext4 /dev/root rw\n\
             5 2 8:2 /folder /run/media/user/Bind rw - ext4 /dev/sdb rw\n\
             6 2 0:21 / /run/media/user/Virtual rw - tmpfs tmpfs rw\n\
             7 3 8:1 /etc /run/media/user/Games/nested rw - ext4 /dev/root rw\n",
        );
        let media = media_roots(&views);
        assert_eq!(media, [PathBuf::from("/run/media/user/Games")]);
        let points: Vec<_> =
            views.iter().map(|view| view.point.clone()).collect();
        assert!(on_media(
            Path::new("/run/media/user/Games/game"),
            &points,
            &media
        ));
        for path in [
            "/run/media/user/Games",
            "/run/media/user/Games/nested/file",
            "/run/media/user/System/etc",
            "/run/media/user/Bind/file",
            "/run/media/user/Virtual/file",
            "/run/media/user/NotMounted/file",
            "/run/user/1000/file",
        ] {
            assert!(!on_media(Path::new(path), &points, &media), "{path}");
        }
        let mut stacked = views.clone();
        let mut covering = views[3].clone();
        covering.point = PathBuf::from("/run/media/user/Games");
        stacked.push(covering);
        assert!(media_roots(&stacked).is_empty(), "overmounted disk");
        assert!(media_roots(&[]).is_empty());
    }

    /// Run in a private mount namespace with the fixture described in
    /// scripts/test-linux-mounts.sh. Never changes the host's mount table.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "requires the isolated mount fixture"]
    fn mounted_media_and_repeated_views() {
        let table =
            fs::read_to_string("/proc/self/mountinfo").expect("mountinfo");
        assert!(
            crate::space::parse_mountinfo(&table)
                .iter()
                .any(|mount| mount.point == Path::new("/run")
                    && mount.source == "disktree-test-run")
        );
        let root = Path::new("/run/media/disktree-test");
        let mounts = read_mount_points();
        // User namespaces cannot mount a block device. Supply the media
        // classification separately; the table-policy test above covers it.
        let media = vec![root.to_path_buf()];
        let file = root.join("game");
        fs::write(&file, b"game").expect("fixture");
        assert_eq!(refuse(&file, root, None, &mounts, &media), None);
        assert!(refuse(root, Path::new("/"), None, &mounts, &media).is_some());
        assert!(refuse(&file, root, None, &mounts, &[]).is_some());
        std::os::unix::fs::symlink("/etc", root.join("link")).expect("link");
        assert!(
            refuse(
                &root.join("link/passwd"),
                &root.join("link"),
                None,
                &mounts,
                &media
            )
            .is_some()
        );
        fs::write(root.join("nested/keep"), b"keep").expect("nested");
        assert!(
            refuse(&root.join("nested/keep"), root, None, &mounts, &media)
                .is_some()
        );
        remove_permanently(&file, root).expect("delete marked game");
        assert!(!file.exists());
        assert!(root.join("nested/keep").exists());

        let scan_root = Path::new("/run/disktree-scan");
        let options = crate::scan::ScanOptions {
            apparent_size: true,
            one_filesystem: false,
            ..crate::scan::ScanOptions::default()
        };
        let tree = crate::scan::scan(scan_root, options.clone()).expect("scan");
        for name in ["one", "two", "original"] {
            assert!(
                tree.children.iter().any(|child| &*child.name == name),
                "{name}"
            );
        }
        assert!(!tree.children.iter().any(|child| &*child.name == "alias"));
        assert_eq!(tree.files, 3);
        assert_eq!(tree.bytes, 12);
        assert!(
            std::process::Command::new("mount")
                .args([
                    "-t",
                    "tmpfs",
                    "tmpfs",
                    "/run/disktree-scan/alias/nested"
                ])
                .status()
                .expect("nested mount")
                .success()
        );
        fs::write(scan_root.join("alias/nested/unique"), b"unique")
            .expect("unique");
        let tree = crate::scan::scan(scan_root, options).expect("rescan");
        let alias = tree
            .children
            .iter()
            .find(|child| &*child.name == "alias")
            .expect("alias retained");
        assert!(
            alias
                .children
                .iter()
                .any(|child| &*child.name == "nested" && child.files == 1)
        );
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

    /// Marks may sit on another volume — `-X` crosses filesystems, and a
    /// disk mounted under the root can be marked — but the meter measures
    /// the scanned volume, so only the marks whose bytes come back to it may
    /// be projected onto its free space.
    #[test]
    fn only_the_scanned_volumes_marks_are_projected() {
        let table = "\
/dev/mapper/root / btrfs rw,subvol=/@ 0 0
/dev/mapper/root /home btrfs rw,subvol=/@home 0 0
/dev/mapper/root /.snapshots btrfs rw,subvol=/@snapshots 0 0
/dev/mapper/data /data ext4 rw 0 0
";
        let mounts = MountTable {
            points: crate::space::parse_mounts(table)
                .iter()
                .map(|mount| guard_key(&mount.point))
                .collect(),
            media: Vec::new(),
            mounts: Some(crate::space::parse_mounts(table)),
        };
        let marked = [
            target(Path::new("/home/tobi/junk"), 100),
            target(Path::new("/data/junk"), 200),
            target(Path::new("/.snapshots/7/junk"), 400),
        ];
        let plan = plan_against(&marked, Path::new("/"), &mounts);
        assert!(
            plan.blocked.is_empty(),
            "a mark inside another volume is legal: {:?}",
            plan.blocked
        );
        assert_eq!(plan.bytes(), 700);
        assert_eq!(plan.reclaim(), 100, "the Btrfs subvolume is this disk");
        assert_eq!(plan.foreign(), 600, "another disk, and a snapshot");
        assert_eq!(plan.unattributed(), 0);

        // A mark inside another is removed with it, so its bytes are counted
        // once here as they are in the total.
        let nested = [
            target(Path::new("/home/tobi/junk"), 100),
            target(Path::new("/home/tobi/junk/deeper"), 60),
        ];
        let plan = plan_against(&nested, Path::new("/"), &mounts);
        assert_eq!(plan.covered.len(), 1);
        assert_eq!(plan.bytes(), 100);
        assert_eq!(plan.reclaim(), 100);

        // Where the table cannot place the paths, nothing is claimed: a
        // projection the meter cannot stand behind is worse than none.
        let partial = MountTable {
            mounts: Some(crate::space::parse_mounts(
                "/dev/mapper/data /data ext4 rw 0 0\n",
            )),
            ..MountTable::default()
        };
        let root = Path::new("/disktree-untabled");
        let plan =
            plan_against(&[target(&root.join("junk"), 50)], root, &partial);
        assert_eq!(plan.bytes(), 50);
        assert_eq!(plan.reclaim(), 0);
        assert_eq!(plan.foreign(), 0);
        assert_eq!(plan.unattributed(), 50, "no saving may be claimed");
    }

    #[test]
    fn the_root_and_home_are_refused() {
        let temp = tree();
        let root = temp.path();
        let home = std::env::home_dir();
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

    /// Removing a directory takes what is inside it, so the directories
    /// above home are refused too: in a whole-disk scan, `C:\Users` is an
    /// ordinary directory, and so is `/home` without a separate volume.
    #[test]
    fn the_directories_above_home_are_refused() {
        let home = Home::of(Path::new("/home/disktree-test-user"));
        let reason =
            refuse(Path::new("/home"), Path::new("/"), Some(&home), &[], &[]);
        assert!(
            reason.is_some_and(|reason| reason.contains("home directory")),
            "the home directory is inside it"
        );
        assert_eq!(
            refuse(
                Path::new("/home/other"),
                Path::new("/"),
                Some(&home),
                &[],
                &[]
            ),
            None,
            "a sibling of home is not above it"
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
    fn a_directory_holding_the_home_directory_is_refused() {
        // /Users in a whole-disk scan on macOS: the same volume as /, so no
        // mount point, and removing it would empty home first.
        let home = Path::new("/Users/tobi");
        let reason = refuse(
            Path::new("/Users"),
            Path::new("/"),
            Some(&Home::of(home)),
            &[],
            &[],
        )
        .expect("refused");
        assert!(reason.contains("home directory"), "{reason}");
        assert!(
            refuse(
                Path::new("/Users/tobi/src"),
                Path::new("/"),
                Some(&Home::of(home)),
                &[],
                &[]
            )
            .is_none(),
            "inside home is the user's"
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn a_directory_holding_a_system_tree_is_refused() {
        let home = Path::new("/Users/tobi");
        for (path, inside) in [("/opt", "/opt/homebrew"), ("/var", "/var/lib")]
        {
            let reason = refuse(
                Path::new(path),
                Path::new("/"),
                Some(&Home::of(home)),
                &[],
                &[],
            )
            .expect("refused");
            assert!(reason.contains(inside), "{path}: {reason}");
        }
        assert!(
            refuse(
                Path::new("/opt/local"),
                Path::new("/"),
                Some(&Home::of(home)),
                &[],
                &[]
            )
            .is_none(),
            "a sibling of a system tree is not its parent"
        );
    }

    #[test]
    fn a_target_with_a_mount_inside_is_refused() {
        let temp = tree();
        let root = temp.path();
        let mounts =
            vec![guard_key(&root.join("a/b")), guard_key(&root.join("other"))];
        assert_eq!(
            mount_below(&guard_key(&root.join("a")), &mounts),
            Some(guard_key(&root.join("a/b")).as_path())
        );
        assert_eq!(
            mount_below(&guard_key(&root.join("a/b")), &mounts),
            None,
            "itself"
        );
        let reason =
            refuse(&root.join("a"), root, None, &mounts, &[]).expect("refused");
        assert!(reason.contains("mounted inside"), "{reason}");
        assert_eq!(
            refuse(&root.join("a/one.bin"), root, None, &mounts, &[]),
            None
        );
    }

    #[test]
    fn nothing_is_mounted_below_a_temp_dir() {
        let temp = tree();
        assert_eq!(
            mount_below(&guard_key(temp.path()), &read_mount_points()),
            None
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn this_macs_mount_table_is_read_without_proc() {
        let mounts = read_mount_points();
        assert!(mounts.contains(&guard_key(Path::new("/"))), "{mounts:?}");
        // Asked of `mount_below` itself: `refuse` would turn /System/Volumes
        // away as a system tree before it looked at the mounts.
        assert!(
            mount_below(&guard_key(Path::new("/System/Volumes")), &mounts)
                .is_some(),
            "the Data volume is mounted there"
        );
    }

    #[test]
    fn the_mount_table_is_read_off_the_calling_thread_and_then_kept() {
        prime_mount_points();
        // The first read lands on its own thread; wait for it, briefly.
        let mut mounts = mount_points();
        for _ in 0..500 {
            if !mounts.points.is_empty() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(10));
            mounts = mount_points();
        }
        assert_eq!(
            mounts.points,
            read_mount_table().points,
            "the same table, cached"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_volume_is_judged_by_its_mount_where_the_kernel_names_one() {
        let temp = tree();
        let dir = open_dir(rustix::fs::CWD, temp.path()).expect("open");
        let here = Volume::of(&dir).expect("volume");
        let mut other = Volume::of(&dir).expect("volume");
        assert!(here.contains(&other));
        // A btrfs subvolume: another device, the same mount.
        other.device.st_dev = other.device.st_dev.wrapping_add(1);
        other.mount = Some(7);
        let mount = Volume {
            device: here.device,
            mount: Some(7),
        };
        assert!(mount.contains(&other), "same mount, other device");
        // A bind mount: the same device, another mount.
        let bind = Volume {
            device: here.device,
            mount: Some(8),
        };
        assert!(!mount.contains(&bind));
        // No mount IDs, as on macOS: the device decides.
        let (mut plain, mut moved) = (
            Volume::of(&dir).expect("volume"),
            Volume::of(&dir).expect("volume"),
        );
        plain.mount = None;
        moved.mount = None;
        moved.device.st_dev = moved.device.st_dev.wrapping_add(1);
        assert!(!plain.contains(&moved));
    }

    #[test]
    fn macos_guard_keys_fold_the_data_volume_and_case() {
        let key = |path: &str| guard_key_for(Path::new(path), true);
        assert_eq!(
            key("/System/Volumes/Data/Users/Tobi/src"),
            PathBuf::from("/users/tobi/src")
        );
        assert_eq!(key("/Library/./Caches"), PathBuf::from("/library/caches"));
        assert_eq!(
            key("/System/Volumes/Data"),
            PathBuf::from("/system/volumes/data"),
            "the Data volume itself stays under /System"
        );
        assert_eq!(
            guard_key_for(Path::new("/Library/X"), false),
            PathBuf::from("/Library/X"),
            "Linux paths are compared as spelled"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_guards_see_through_other_spellings() {
        let home = Path::new("/Users/tobi");
        let data = Path::new(crate::space::MACOS_DATA_VOLUME);
        assert!(
            refuse(
                &data.join("Users/tobi/src"),
                data,
                Some(&Home::of(home)),
                &[],
                &[]
            )
            .is_none(),
            "home under the Data volume's name is still home"
        );
        let reason = refuse(
            Path::new("/library/caches"),
            Path::new("/"),
            Some(&Home::of(home)),
            &[],
            &[],
        )
        .expect("refused");
        assert!(reason.contains("/Library"), "{reason}");
    }

    #[test]
    fn permanent_removal_takes_nested_trees_and_hidden_files() {
        let temp = tree();
        let doomed = temp.path().join("a");
        fs::create_dir_all(doomed.join("b/c/d")).expect("mkdir");
        fs::write(doomed.join("b/c/d/.hidden"), "x").expect("write");
        fs::write(doomed.join("b/c/f"), "x").expect("write");
        remove_permanently(&doomed, temp.path()).expect("remove tree");
        assert!(!doomed.exists());
        assert!(temp.path().join("other").exists());
    }

    #[test]
    fn permanent_removal_unlinks_nested_symlinks_without_following_them() {
        let temp = tree();
        let outside = TempDir::new().expect("tempdir");
        fs::write(outside.path().join("keep"), "x").expect("write");
        link_dir(outside.path(), &temp.path().join("a/link"));
        remove_permanently(&temp.path().join("a"), temp.path())
            .expect("remove tree");
        assert!(outside.path().join("keep").exists());
    }

    #[test]
    fn permanent_removal_takes_directories_and_leaves_siblings() {
        let temp = tree();
        let doomed = temp.path().join("a/b");
        remove_permanently(&doomed, temp.path()).expect("remove dir");
        assert!(!doomed.exists());
        assert!(temp.path().join("a/c.bin").exists());
    }

    /// A link to a directory: a symbolic link on Unix, a junction on
    /// Windows, which needs no privilege to make.
    fn link_dir(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).expect("symlink");
        #[cfg(windows)]
        assert!(crate::windows::make_junction(link, target), "junction");
    }

    #[test]
    fn permanent_removal_unlinks_a_symlink_instead_of_following_it() {
        let temp = tree();
        let keep = TempDir::new().expect("tempdir");
        fs::write(keep.path().join("precious.bin"), b"data").expect("write");
        let link = temp.path().join("link");
        link_dir(keep.path(), &link);

        remove_permanently(&link, temp.path()).expect("remove link");
        assert!(fs::symlink_metadata(&link).is_err(), "the link is gone");
        assert!(keep.path().join("precious.bin").exists());
    }

    /// Marked as `a/b/x`, then `a/b` swapped for a link to somewhere else
    /// holding an `x`: that other `x` must survive.
    #[test]
    #[cfg(unix)]
    fn permanent_removal_does_not_follow_a_link_put_on_the_way_down() {
        let temp = tree();
        let keep = TempDir::new().expect("tempdir");
        fs::create_dir(keep.path().join("x")).expect("mkdir");
        fs::write(keep.path().join("x/precious.bin"), b"data").expect("write");
        let marked = temp.path().join("a/b/x");
        fs::remove_dir_all(temp.path().join("a/b")).expect("clear");
        link_dir(keep.path(), &temp.path().join("a/b"));

        let error =
            remove_permanently(&marked, temp.path()).expect_err("refused");
        assert!(error.to_string().contains("changed since"), "{error}");
        assert!(keep.path().join("x/precious.bin").exists());
    }

    #[test]
    fn a_target_reached_through_a_link_is_refused() {
        let temp = tree();
        let keep = TempDir::new().expect("tempdir");
        fs::write(keep.path().join("precious.bin"), b"data").expect("write");
        link_dir(keep.path(), &temp.path().join("link"));

        let plan = plan(
            &[target(&temp.path().join("link/precious.bin"), 4)],
            temp.path(),
        );
        assert!(plan.is_empty());
        assert!(
            plan.blocked[0].reason.contains("through a link"),
            "{}",
            plan.blocked[0].reason
        );
    }

    /// The trash follows links on the way down, so the worker asks again,
    /// whatever the mode.
    #[test]
    fn a_link_put_on_the_way_down_after_planning_stops_the_worker() {
        let temp = tree();
        let keep = TempDir::new().expect("tempdir");
        fs::create_dir(keep.path().join("x")).expect("mkdir");
        fs::write(keep.path().join("x/precious.bin"), b"data").expect("write");
        fs::create_dir(temp.path().join("a/b/x")).expect("mkdir");
        let plan = plan(&[target(&temp.path().join("a/b/x"), 0)], temp.path());
        assert_eq!(plan.targets.len(), 1);
        fs::remove_dir_all(temp.path().join("a/b")).expect("clear");
        link_dir(keep.path(), &temp.path().join("a/b"));

        let handle = spawn(plan, RemovalMode::Permanent);
        let mut outcome = None;
        for _ in 0..2000 {
            while let Some(event) = handle.poll() {
                if let RemovalEvent::Item { outcome: item, .. } = event {
                    outcome = Some(item);
                }
            }
            if outcome.is_some() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(1));
        }
        let error = outcome.expect("an item").expect_err("refused");
        assert!(error.contains("through a link"), "{error}");
        assert!(keep.path().join("x/precious.bin").exists());
    }

    #[test]
    fn home_is_found_by_its_real_path_too() {
        let temp = tree();
        let real = temp.path().join("data");
        fs::create_dir_all(real.join("home/user")).expect("mkdir");
        let spelled = TempDir::new().expect("tempdir");
        link_dir(&real.join("home"), &spelled.path().join("home"));
        let home = Home::of(&spelled.path().join("home/user"));
        let root = fs::canonicalize(temp.path()).expect("real root");

        let reason =
            linked(&root.join("data"), &root, Some(&root), Some(&home))
                .expect("refused");
        assert!(reason.contains("home directory"), "{reason}");
        assert_eq!(
            linked(&root.join("other"), &root, Some(&root), Some(&home)),
            None
        );
    }

    #[test]
    fn names_win32_would_misread_are_recognized() {
        for name in [
            "Users.",
            "Users ",
            "a. .",
            "NUL",
            "nul.txt",
            "Com1",
            "LPT9.log",
            "con ",
            "file:stream",
            "COM\u{b9}",
        ] {
            assert!(misread_by_win32(name), "{name:?}");
        }
        for name in ["Users", ".git", "..hidden", "console", "COM0", "LPT10"] {
            assert!(!misread_by_win32(name), "{name:?}");
        }
    }

    /// On macOS the temporary directory is under `/var`, itself a link to
    /// `/private/var`: a link above the scanned root is how it was named.
    #[test]
    fn permanent_removal_follows_links_above_the_root() {
        let temp = tree();
        let spelled = TempDir::new().expect("tempdir");
        let root = spelled.path().join("root");
        link_dir(temp.path(), &root);

        remove_permanently(&root.join("a/b"), &root).expect("removed");
        assert!(!temp.path().join("a/b").exists());
        assert!(temp.path().join("a/one.bin").exists());
    }

    #[test]
    fn a_mount_below_a_target_is_caught_again_at_removal_time() {
        let temp = tree();
        let root = temp.path();
        let mounts = vec![guard_key(&root.join("a/b"))];
        let reason = mounted(&root.join("a"), &mounts).expect("caught");
        assert!(reason.contains("mounted inside"), "{reason}");
        assert_eq!(mounted(&root.join("other"), &mounts), None);
    }

    /// Git keeps its objects read-only, which on Windows stops a plain
    /// delete; clearing an old checkout must still work.
    #[test]
    fn read_only_files_are_removed_too() {
        let temp = tree();
        let objects = temp.path().join("a/b/objects");
        fs::create_dir_all(&objects).expect("mkdir");
        let object = objects.join("pack.idx");
        fs::write(&object, b"x").expect("write");
        let mut permissions =
            fs::metadata(&object).expect("stat").permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&object, permissions).expect("read-only");
        let lone = temp.path().join("a/lone.idx");
        fs::write(&lone, b"x").expect("write");
        let mut permissions = fs::metadata(&lone).expect("stat").permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&lone, permissions).expect("read-only");

        remove_permanently(&lone, temp.path()).expect("a read-only file");
        remove_permanently(&temp.path().join("a/b"), temp.path())
            .expect("a directory");
        assert!(!lone.exists());
        assert!(!temp.path().join("a/b").exists());
        assert!(temp.path().join("a/c.bin").exists());
    }

    #[test]
    fn a_missing_target_reports_not_found() {
        let temp = tree();
        let error =
            remove_permanently(&temp.path().join("absent"), temp.path())
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(unix)]
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

    #[cfg(not(windows))]
    #[test]
    #[cfg(not(any(target_os = "macos", windows)))]
    fn detection_prefers_a_tool_this_machine_has() {
        let backend = detect_trash_backend();
        if which("trash-put") {
            assert_eq!(backend, TrashBackend::TrashPut);
        }
        assert!(backend.is_available());
    }

    #[cfg(windows)]
    #[test]
    fn windows_trashes_into_the_recycle_bin() {
        let backend = detect_trash_backend();
        assert_eq!(backend, TrashBackend::RecycleBin);
        assert!(backend.is_available());
    }

    #[cfg(windows)]
    #[test]
    fn windows_system_trees_are_refused_in_a_whole_disk_scan() {
        let drive = system_drive();
        let home = drive.join(r"Users\tobi");
        let system = |relative: &str| {
            system_tree(
                &guard_key(&drive.join(relative)),
                Some(&guard_key(&home)),
            )
            .map(str::to_lowercase)
        };
        let windows = drive.join("Windows").display().to_string();
        assert!(
            system(r"WINDOWS\System32\drivers\etc\hosts").is_some(),
            "without regard to case"
        );
        assert!(system(r"Program Files\App\app.exe").is_some());
        assert!(system(r"Program Files (x86)\App").is_some());
        assert!(system(r"ProgramData\Vendor\state.db").is_some());
        assert!(system("System Volume Information").is_some());
        assert!(system("pagefile.sys").is_some());
        assert!(system("HIBERFIL.SYS").is_some());
        assert_eq!(system("Windows.old"), None, "components, not prefixes");
        assert_eq!(system(r"Users\tobi\AppData\Local\Temp"), None);
        assert_eq!(system("Games"), None);
        let reason = refuse(
            &drive.join(r"Windows\Temp"),
            &drive,
            Some(&Home::of(&home)),
            &[],
            &[],
        );
        assert!(
            reason.is_some_and(|reason| reason
                .to_lowercase()
                .contains(&windows.to_lowercase())),
            "refused as part of {windows}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_profiles_are_refused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let profiles = temp.path();
        fs::create_dir_all(profiles.join(r"tobi\Downloads")).expect("mkdir");
        fs::write(profiles.join("notes.txt"), b"x").expect("write");
        let profile = |relative: &str| {
            let path = profiles.join(relative);
            is_profile(&path, &guard_key(&path), &guard_key(profiles))
        };
        assert!(profile("tobi"));
        assert!(profile("TOBI"), "compared as Windows compares names");
        assert!(!profile("notes.txt"), "a file there is no profile");
        assert!(!profile(r"tobi\Downloads"), "inside a profile is not one");
        assert!(!profile("gone"), "nothing there is no profile");

        // The rule itself, against the profiles folder Windows reports.
        let public = crate::windows::user_profiles_dir()
            .expect("profiles folder")
            .join("Public");
        let reason = refuse(&public, &system_drive(), None, &[], &[]);
        assert!(reason.is_some_and(|reason| reason.contains("profile")));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_always_uses_the_system_trash() {
        // Whatever Homebrew put on PATH: its trash tools write a trash
        // Finder never shows.
        assert_eq!(detect_trash_backend(), TrashBackend::MacOs);
        assert!(TrashBackend::MacOs.is_available());
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "puts a file in this Mac's real Trash"]
    fn the_macos_trash_takes_a_file_and_leaves_a_linked_target() {
        let temp = TempDir::new().expect("tempdir");
        let file = temp.path().join("disktree-trash-check.txt");
        fs::write(&file, b"x").expect("write");
        let keep = temp.path().join("keep.txt");
        fs::write(&keep, b"kept").expect("write");
        let link = temp.path().join("disktree-trash-check-link");
        std::os::unix::fs::symlink(&keep, &link).expect("symlink");

        move_to_trash(&file, TrashBackend::MacOs).expect("trashed");
        move_to_trash(&link, TrashBackend::MacOs).expect("trashed");

        assert!(!file.exists());
        assert!(fs::symlink_metadata(&link).is_err(), "the link went");
        assert_eq!(fs::read(&keep).expect("read"), b"kept", "not its target");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn a_name_that_is_not_utf8_is_refused_before_the_trash_sees_it() {
        use std::os::unix::ffi::OsStrExt as _;
        let temp = TempDir::new().expect("tempdir");
        let odd = temp
            .path()
            .join(std::ffi::OsStr::from_bytes(b"not-\xff-utf8"));
        let error = trash_via_macos(&odd).expect_err("refused");
        assert!(error.to_string().contains("UTF-8"), "{error}");
    }

    #[test]
    #[cfg(not(windows))]
    fn system_trees_are_refused_in_a_whole_disk_scan() {
        let home = Path::new("/home/tobi");
        assert_eq!(
            tree_of(Path::new("/usr/lib/libfoo.so"), home),
            Some("/usr")
        );
        assert_eq!(
            tree_of(Path::new("/var/lib/pacman"), home),
            Some("/var/lib")
        );
        assert_eq!(tree_of(Path::new("/var/cache/pacman/pkg"), home), None);
        assert_eq!(tree_of(Path::new("/opt/thing"), home), None);
        assert_eq!(
            tree_of(Path::new("/home/linuxbrew/.linuxbrew/Cellar"), home),
            Some("/home/linuxbrew/.linuxbrew")
        );
        assert_eq!(
            tree_of(Path::new("/gnu/store/abc-hello"), home),
            Some("/gnu/store")
        );
        assert_eq!(
            tree_of(Path::new("/usrlocal"), home),
            None,
            "components, not prefixes"
        );
        let odd_home = Path::new("/usr/home/tobi");
        assert_eq!(tree_of(Path::new("/usr/home/tobi/.cache"), odd_home), None);
        let reason = refuse(
            Path::new("/etc/hosts"),
            Path::new("/"),
            Some(&Home::of(home)),
            &[],
            &[],
        );
        assert!(reason.is_some_and(|reason| reason.contains("/etc")));
    }

    #[test]
    #[cfg(not(windows))]
    fn macos_system_trees_are_refused_but_apps_are_not() {
        let home = Path::new("/Users/tobi");
        for (path, tree) in [
            ("/System/Library/Fonts", "/System"),
            ("/Library/Caches/com.apple.x", "/Library"),
            ("/private/etc/hosts", "/private"),
            ("/private/var/folders/xy", "/private"),
            ("/opt/homebrew/Cellar/git", "/opt/homebrew"),
        ] {
            assert_eq!(tree_of(Path::new(path), home), Some(tree));
        }
        // An app is uninstalled by moving it to the Trash, and a user's own
        // Library is theirs.
        for path in ["/Applications/Xcode.app", "/Users/tobi/Library/Caches"] {
            assert_eq!(tree_of(Path::new(path), home), None, "{path}");
        }
    }
}

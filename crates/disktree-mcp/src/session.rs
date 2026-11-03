//! What one connected agent knows: the window it is driving, or failing
//! that its own scans, and the marks.
//!
//! When a PwrDisk window is linked it is the source of truth: `ls`, `find`
//! and `scan` read the tree the person is looking at, and the marks live
//! there. Without a window this process scans for itself, so an agent in a
//! terminal needs nothing else. Either way the words an agent reads come
//! from here, over `disktree-core` and the link; [`crate::server`] is the
//! thin layer that turns them into MCP tools.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use disktree_core::classify::Category;
use disktree_core::insights::{Candidate, worth_a_look};
use disktree_core::query::{self, Kind};
use disktree_core::removal::{
    Plan, Target, detect_trash_backend, normalize, plan,
};
use disktree_core::scan::{ScanHandle, ScanOptions};
use disktree_core::sharing::{self, Estimate};
use disktree_core::size::{human_bytes, human_count, share};
use disktree_core::space::{
    SpaceInfo, device_for, space_info, volume_root_for, volumes,
};
use disktree_core::tree::{Node, Tree, path_of};
use disktree_link::{
    ConfirmedRemoval, FindRequest, FoundList, Insight, Link, Listing,
    MarkRequest, RemovalChoice, RemovalPreview, Request, Response,
    ScanOptionsRequest, SelectionEstimate, Space, State, TreeSummary, VERSION,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::report::{age, children, clone_summary, describe, display, matches};

/// The measurement context survives a window scan being reused by an agent.
fn scan_context(tree: &TreeSummary) -> String {
    format!(
        "{} sizes \u{00b7} hidden entries {} \u{00b7} {} unreadable",
        if tree.apparent_size {
            "apparent"
        } else {
            "allocated"
        },
        if tree.include_hidden {
            "included"
        } else {
            "skipped"
        },
        tree.unreadable,
    )
}

/// Listed allocation is additive; APFS physical usage is not.
const fn allocation_caveat() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS: summed allocation can exceed volume usage because APFS clones \
         share blocks. Listed bytes are not guaranteed freed space; snapshots \
         can retain deleted blocks.\n"
    } else {
        ""
    }
}

/// How many findings a scan report lists.
const INSIGHTS: usize = 8;
/// Top-level rows a scan report shows.
const TOP: usize = 12;
/// Rows a listing shows unless asked otherwise.
const DEFAULT_LIMIT: usize = 20;
/// The most rows any listing shows: past this the agent should filter.
const MAX_LIMIT: usize = 200;
/// What `show` and `review` say on remaining unsupported targets:
/// the person takes the list over by hand.
const NO_LINK: &str = "This system has no window link yet, so PwrDisk \
cannot show these marks in a window or open its review screen for them. \
Give the user the marked paths and the plan, and ask them to open PwrDisk, \
scan the same root, mark those paths and review them there. Do not remove \
them yourself.";
/// How long to wait for a window that was just launched to start listening.
const LAUNCH_WAIT: Duration = Duration::from_secs(20);
/// A window that did not answer is not launched again sooner than this: it
/// may still be coming up, and a second one would only compete for the
/// socket.
const RELAUNCH_AFTER: Duration = Duration::from_mins(1);

/// The system this server runs on, and the skill reference written for it.
fn system_line() -> String {
    let system = match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    };
    crate::skills::platform_reference().map_or_else(
        || format!("System: {system}.\n"),
        |path| {
            format!(
                "System: {system}. Before marking, read {}: what this \
                 system hides from a scan and what never to mark.\n",
                crate::skills::uri("pwrdisk-free-space", path)
            )
        },
    )
}

/// Unix seconds now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs().cast_signed())
}

/// Starts a PwrDisk window on a root, for the review screen when none is
/// open. Swapped out in tests.
pub type Launcher = Box<dyn Fn(&Path) -> io::Result<()> + Send>;

/// One finished scan of this process's own, kept for the rest of the
/// session.
#[derive(Debug)]
pub struct Scan {
    pub root: PathBuf,
    pub tree: Arc<Tree>,
    pub options: ScanOptions,
    /// Unix seconds when the tree landed.
    pub at: i64,
    pub elapsed: Duration,
    pub unreadable: u64,
    pub space: Option<SpaceInfo>,
    pub device: Option<String>,
    pub insights: Vec<Candidate>,
}

/// What a scan request amounts to, decided with the session locked.
#[derive(Debug)]
pub enum ScanPlan {
    /// Walk here, off the protocol thread, then [`Session::store_scan`].
    Local { root: PathBuf, options: ScanOptions },
    /// The window is scanning `root`; poll [`Session::window_scan`] until
    /// it reports.
    Window { root: PathBuf },
}

/// What `pwrdisk_scan` takes.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScanParams {
    /// Directory to scan. Default: the home directory.
    pub path: Option<PathBuf>,
    /// Scan the whole volume that `path` (or the home directory) is on.
    #[serde(default)]
    pub disk: bool,
    /// Measure apparent length, as `ls -l` shows it, instead of allocated
    /// blocks. Blocks are what the disk spends and what deleting frees.
    #[serde(default)]
    pub apparent_size: bool,
    /// Skip dotfiles and dot-directories. Off by default: `~/.cache` is
    /// often the largest thing in a home directory.
    #[serde(default)]
    pub skip_hidden: bool,
    /// Also measure other disks, network shares and pseudo filesystems
    /// mounted below the root. Off by default: one volume at a time.
    #[serde(default)]
    pub cross_filesystems: bool,
    /// Stop descending past this depth. Totals below it are unknown, which
    /// makes a first look at a huge tree cheap.
    pub max_depth: Option<usize>,
}

/// What `pwrdisk_ls` takes.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LsParams {
    /// The directory to list, under the scanned root.
    pub path: PathBuf,
    /// How many entries to list, largest first. Default 20, at most 200.
    pub limit: Option<usize>,
    /// Legacy input, accepted without moving the window. Use `pwrdisk_show`
    /// only when the user explicitly asks to see a path.
    #[serde(default)]
    #[schemars(skip)]
    pub show: bool,
}

/// A hypothetical selection; the marked cart is never used or modified.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EstimateParams {
    /// 1 to 200 paths from one completed scan. Overlaps are covered once.
    pub paths: Vec<PathBuf>,
    /// Group retained clones by direct child directories of this scanned
    /// directory. Default: the selected paths' common parent. Widen this
    /// scope to search elsewhere in the scan; no owner discovery outside it.
    pub peer_scope: Option<PathBuf>,
    /// Maximum directory peers, default 10, at most 20. Zero skips peers.
    pub peer_limit: Option<usize>,
}

/// What kind of entry `pwrdisk_find` matches; [`Kind`] as a parameter.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "lowercase")]
pub enum KindName {
    /// Files and directories; a matching directory hides its contents.
    #[default]
    Any,
    File,
    Dir,
}

impl From<KindName> for Kind {
    fn from(name: KindName) -> Self {
        match name {
            KindName::Any => Self::Any,
            KindName::File => Self::File,
            KindName::Dir => Self::Dir,
        }
    }
}

/// A kind of data, as `pwrdisk_find` filters it; [`Category`] as a
/// parameter.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CategoryName {
    Code,
    AgentScratch,
    Toolchain,
    Synced,
    Git,
    Media,
    Documents,
    Cache,
    Other,
}

impl From<CategoryName> for Category {
    fn from(name: CategoryName) -> Self {
        match name {
            CategoryName::Code => Self::Code,
            CategoryName::AgentScratch => Self::AgentScratch,
            CategoryName::Toolchain => Self::Toolchain,
            CategoryName::Synced => Self::Synced,
            CategoryName::Git => Self::Git,
            CategoryName::Media => Self::Media,
            CategoryName::Documents => Self::Documents,
            CategoryName::Cache => Self::Cache,
            CategoryName::Other => Self::Other,
        }
    }
}

/// An explicit removal mode; never inferred from a default or availability.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemovalModeName {
    Trash,
    Permanent,
}

impl From<RemovalModeName> for RemovalChoice {
    fn from(mode: RemovalModeName) -> Self {
        match mode {
            RemovalModeName::Trash => Self::Trash,
            RemovalModeName::Permanent => Self::Permanent,
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemovalPreviewParams {
    /// Required: trash (possibly recoverable) or permanent (cannot be undone).
    pub mode: RemovalModeName,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoveParams {
    /// Single-use ID returned by `pwrdisk_removal_preview`. Expires in 5 minutes.
    pub preview_id: String,
    /// Must match the mode the user explicitly approved from the preview.
    pub mode: RemovalModeName,
    /// Exact ordered path list returned by the preview and approved by the user.
    pub paths: Vec<PathBuf>,
    /// Only after explicit user confirmation: MOVE TO TRASH or PERMANENTLY DELETE,
    /// matching the preview. Approval to mark is not approval to remove.
    pub confirmation: String,
    /// Rescan once the removal finishes. Default false, so the next batch
    /// can be found and marked at once; a rescan of a large volume takes
    /// minutes. Until one, removed paths are still listed, and the window
    /// refuses to mark them.
    #[serde(default)]
    pub rescan: bool,
}

/// What `pwrdisk_find` takes.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindParams {
    /// Where to search. Default: the whole scanned tree.
    pub path: Option<PathBuf>,
    /// `file`, `dir` or `any` (default). A matching directory is listed
    /// once, without what is inside it.
    pub kind: Option<KindName>,
    /// Only entries of at least this many bytes.
    pub min_bytes: Option<u64>,
    /// Only this kind of data.
    pub category: Option<CategoryName>,
    /// Cleanup candidates: caches, build output, package stores, installed
    /// dependencies, sync history, trash, scratch. This classification is
    /// not removal eligibility; results report removal guard refusals.
    #[serde(default)]
    pub reclaimable: bool,
    /// Only names containing this text, case-insensitively.
    pub name: Option<String>,
    /// Only entries not written to in this many days. For a directory, the
    /// newest write anywhere beneath it counts.
    pub older_than_days: Option<u32>,
    /// How many to list, largest first. Default 25, at most 200.
    pub limit: Option<usize>,
}

impl FindParams {
    /// The same search as the link words it, with `now` fixing what
    /// `older_than_days` means.
    fn request(&self, path: Option<PathBuf>, now: i64) -> FindRequest {
        FindRequest {
            path,
            kind: Kind::from(self.kind.unwrap_or_default()).name().to_string(),
            min_bytes: self.min_bytes.unwrap_or(0),
            category: self
                .category
                .map(|name| Category::from(name).name().to_string()),
            reclaimable: self.reclaimable,
            name: self.name.clone(),
            before: self
                .older_than_days
                .map(|days| now - i64::from(days) * 86_400),
            limit: self.limit.unwrap_or(25).clamp(1, MAX_LIMIT),
        }
    }
}

/// What `pwrdisk_mark` and `pwrdisk_unmark` take.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathsParams {
    /// Absolute paths under the scanned root. For `pwrdisk_unmark`, an
    /// empty list unmarks everything.
    #[serde(default)]
    pub paths: Vec<PathBuf>,
}

/// What `pwrdisk_show` takes.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShowParams {
    /// The path to show in the window, under the scanned root.
    pub path: PathBuf,
}

/// One connected agent's state.
pub struct Session {
    home: Option<PathBuf>,
    socket: PathBuf,
    launcher: Option<Launcher>,
    client: String,
    /// This process's own scans, read only while no window is linked.
    scans: Vec<Scan>,
    /// Marks as this session knows them: the window's when one is linked,
    /// else its own.
    marks: Vec<Target>,
    link: Option<Link>,
    /// What the window last said about itself.
    window: Option<State>,
    /// Why an open window is not linked: it shows an SSH host, speaks
    /// another link version, or refused the marks made here. Said instead
    /// of starting a window that could not link either.
    unlinked: Option<String>,
    /// When a window was last started from here, so one that is slow to
    /// come up is not started twice.
    launched: Option<Instant>,
    pending_removal: Option<RemovalPreview>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("client", &self.client)
            .field("scans", &self.scans.len())
            .field("marks", &self.marks.len())
            .field("linked", &self.link.is_some())
            .finish_non_exhaustive()
    }
}

/// Start a PwrDisk window on `root`, detached from this process so the
/// agent's exit does not take the window with it.
fn launch_window(root: &Path) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Its own process group: a signal to the agent's group stops here.
        command.process_group(0);
    }
    command.spawn()?;
    Ok(())
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    /// A session over the real socket, able to launch a window.
    pub fn new() -> Self {
        // Where no window can be linked, starting one would only leave a
        // window the agent cannot reach, and another on every retry.
        let launcher: Option<Launcher> = if disktree_link::SUPPORTED {
            Some(Box::new(launch_window))
        } else {
            None
        };
        Self::detached(disktree_link::socket_path(), launcher)
    }

    /// A session over `socket`, launching windows with `launcher` or not at
    /// all. Tests use this with a stand-in for the window.
    pub fn detached(socket: PathBuf, launcher: Option<Launcher>) -> Self {
        Self {
            home: std::env::home_dir(),
            socket,
            launcher,
            client: "an agent".to_string(),
            scans: Vec::new(),
            marks: Vec::new(),
            link: None,
            window: None,
            unlinked: None,
            launched: None,
            pending_removal: None,
        }
    }

    /// Remember who is connected, by the name a client gives at
    /// `initialize`, in the words the window shows.
    pub fn set_client(&mut self, name: &str) {
        let lowered = name.to_ascii_lowercase();
        self.client = if lowered.contains("claude-code")
            || lowered.contains("claude code")
        {
            "Claude Code".to_string()
        } else if lowered.contains("claude") {
            "Claude".to_string()
        } else if lowered.contains("codex") {
            "Codex".to_string()
        } else if name.trim().is_empty() {
            "an agent".to_string()
        } else {
            name.to_string()
        };
    }

    pub fn client(&self) -> &str {
        &self.client
    }

    pub fn scans(&self) -> &[Scan] {
        &self.scans
    }

    pub fn marks(&self) -> &[Target] {
        &self.marks
    }

    fn show(&self, path: &Path) -> String {
        display(path, self.home.as_deref())
    }

    // ── scanning ────────────────────────────────────────────────────────

    /// Decide what a scan request means: the root and the options, and
    /// whether the window or this process does the walking. With a window
    /// linked the scan is asked of it here, and [`Self::window_scan`]
    /// follows it; otherwise the walk runs with the session unlocked.
    pub fn prepare_scan(
        &mut self,
        params: &ScanParams,
    ) -> Result<ScanPlan, String> {
        let base = match &params.path {
            Some(path) => self.absolute(path)?,
            None => self
                .home
                .clone()
                .ok_or("no path given and no home directory")?,
        };
        let root = if params.disk {
            volume_root_for(&base).unwrap_or_else(|| PathBuf::from("/"))
        } else {
            base
        };
        let metadata = std::fs::metadata(&root).map_err(|error| {
            format!("cannot read {}: {error}", root.display())
        })?;
        if !metadata.is_dir() {
            return Err(format!("{} is not a directory", root.display()));
        }
        let options = ScanOptions {
            apparent_size: params.apparent_size,
            include_hidden: !params.skip_hidden,
            one_filesystem: !params.cross_filesystems,
            max_depth: params.max_depth,
            ..ScanOptions::default()
        };
        let request = Request::Scan {
            root: root.clone(),
            options: ScanOptionsRequest {
                apparent_size: Some(options.apparent_size),
                include_hidden: Some(options.include_hidden),
                one_filesystem: Some(options.one_filesystem),
                max_depth: Some(options.max_depth),
            },
        };
        if self.try_connect_for_request(Some(&request))? {
            Ok(ScanPlan::Window { root })
        } else {
            Ok(ScanPlan::Local { root, options })
        }
    }

    /// Whether the window has finished the scan of `root` it was asked
    /// for, and if so what it found. `Ok(None)` is "still scanning".
    pub fn window_scan(
        &mut self,
        root: &Path,
    ) -> Result<Option<String>, String> {
        let state = match self.call(&Request::State)? {
            Response::State(state) => state,
            other => return Err(unexpected(&other)),
        };
        if state.root != root {
            return Err(format!(
                "The window moved to {} before the scan of {} finished; \
                 the user may have gone elsewhere. Scan again if needed.",
                self.show(&state.root),
                self.show(root)
            ));
        }
        if state.scanning {
            return Ok(None);
        }
        let Some(tree) = state.tree.clone() else {
            return Err(format!(
                "The window has no tree for {}: its scan did not finish. \
                 The window shows why.",
                self.show(root)
            ));
        };
        let top = match self.call(&Request::Ls {
            path: root.to_path_buf(),
            limit: TOP,
        })? {
            Response::Listing(listing) => listing,
            Response::Error { message } => return Err(message),
            other => return Err(unexpected(&other)),
        };
        let facts = ScanFacts {
            root: root.to_path_buf(),
            tree,
            space: state.space,
            device: device_for(root),
            top,
            insights: state.insights,
        };
        let mut report = self.scan_report(&facts);
        report.push('\n');
        report.push_str(&self.window_line());
        Ok(Some(report))
    }

    /// Keep a finished scan of this process's own, replacing an earlier one
    /// of the same root, and say what it found.
    pub fn store_scan(&mut self, scan: Scan) -> String {
        self.scans.retain(|known| known.root != scan.root);
        let report = self.scan_report(&ScanFacts::of_local(&scan));
        self.scans.push(scan);
        report
    }

    fn scan_report(&self, facts: &ScanFacts) -> String {
        let now = now();
        let tree = &facts.tree;
        let mut out = String::new();
        let _ = write!(
            out,
            "Scanned {} in {:.1} s: {} \u{00b7} {} files \u{00b7} {} \
             directories",
            self.show(&facts.root),
            Duration::from_millis(tree.elapsed_ms).as_secs_f64(),
            human_bytes(tree.bytes),
            human_count(tree.files),
            human_count(tree.dirs),
        );
        let _ = writeln!(out, " \u{00b7} {}", scan_context(tree));
        if let Some(space) = facts.space {
            let used = space.total.saturating_sub(space.free);
            let _ = writeln!(
                out,
                "Volume {}{}: {} free of {}, {:.0}% used. This scan holds \
                 {:.0}% of what is used; the rest is outside {} or is not \
                 files (snapshots, other volumes, swap).",
                volume_root_for(&facts.root).map_or_else(
                    || "?".to_string(),
                    |root| root.display().to_string()
                ),
                facts
                    .device
                    .as_deref()
                    .map_or_else(String::new, |device| format!(" ({device})")),
                human_bytes(space.available),
                human_bytes(space.total),
                share(used, space.total.max(1)),
                share(tree.bytes, used.max(1)),
                self.show(&facts.root),
            );
        }
        out.push_str(allocation_caveat());
        if let Some(line) = clone_summary(&facts.top.entry) {
            out.push_str(&line);
        }
        out.push_str("Top level, largest first:\n");
        out.push_str(&children(&facts.top, now));
        if !facts.insights.is_empty() {
            out.push_str(
                "Worth a look, the biggest things that could plausibly go:\n",
            );
            for insight in &facts.insights {
                let _ = writeln!(
                    out,
                    "  {:>9}  {}  {}",
                    human_bytes(insight.bytes),
                    self.show(&insight.path),
                    insight.what,
                );
            }
        }
        out.push_str(
            "Next: pwrdisk_ls goes into a directory, pwrdisk_find lists \
             the largest files or every cache, pwrdisk_mark marks what \
             should go.",
        );
        out
    }

    // ── looking ─────────────────────────────────────────────────────────

    /// An absolute, normalized path: `~` expanded, relative to the working
    /// directory otherwise.
    fn absolute(&self, path: &Path) -> Result<PathBuf, String> {
        let expanded = if let Ok(rest) = path.strip_prefix("~") {
            let home = self.home.clone().ok_or("no home directory for ~")?;
            home.join(rest)
        } else if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| format!("no working directory: {error}"))?
                .join(path)
        };
        Ok(normalize(&expanded))
    }

    /// The scan of this process's own holding `path`, with the node there.
    fn locate(&self, path: &Path) -> Result<Located<'_>, String> {
        let path = self.absolute(path)?;
        if self.scans.is_empty() {
            return Err(
                "Nothing is scanned yet. Call pwrdisk_scan first; with no \
                 arguments it scans the home directory."
                    .to_string(),
            );
        }
        let scan = self
            .scans
            .iter()
            .filter(|scan| path.starts_with(&scan.root))
            .max_by_key(|scan| scan.root.as_os_str().len())
            .ok_or_else(|| {
                let roots: Vec<String> = self
                    .scans
                    .iter()
                    .map(|scan| self.show(&scan.root))
                    .collect();
                format!(
                    "{} is outside the scanned roots ({}). Scan a directory \
                     that contains it.",
                    self.show(&path),
                    roots.join(", ")
                )
            })?;
        let relative = path
            .strip_prefix(&scan.root)
            .map_err(|_| "unreachable: prefix was checked".to_string())?;
        let (crumbs, node) =
            scan.tree.root().descend(relative).ok_or_else(|| {
                format!(
                    "{} is not in the scan of {}: it may be new, hidden, below \
                 the depth limit, or on another filesystem. Scan again if \
                 it should be there.",
                    self.show(&path),
                    self.show(&scan.root)
                )
            })?;
        Ok(Located {
            scan,
            path,
            crumbs,
            node,
        })
    }

    /// The window's root when it covers `path`; otherwise why the agent
    /// should scan from higher up. Only meaningful while linked.
    fn window_root_for(&self, path: &Path) -> Result<PathBuf, String> {
        let Some(state) = &self.window else {
            return Err("the window has not said where it is".to_string());
        };
        // A window still on the source chooser has no root, and an empty
        // path is a prefix of every path.
        if state.root.as_os_str().is_empty() {
            return Err(format!(
                "The PwrDisk window has no tree yet: it is waiting for a \
                 source. Call pwrdisk_scan on a directory containing {} and \
                 the window scans it.",
                self.show(path)
            ));
        }
        if path.starts_with(&state.root) {
            return Ok(state.root.clone());
        }
        Err(format!(
            "{} is outside the PwrDisk window's tree, which is rooted at \
             {}. The window is what the user sees, so it is what you read: \
             call pwrdisk_scan on a directory containing it and the window \
             follows.",
            self.show(path),
            self.show(&state.root)
        ))
    }

    /// A directory and its largest children, from the window or from a
    /// scan of this process's own.
    fn listing(
        &mut self,
        path: &Path,
        limit: usize,
    ) -> Result<Listing, String> {
        if self.try_connect() {
            self.window_root_for(path)?;
            return match self.call(&Request::Ls {
                path: path.to_path_buf(),
                limit,
            })? {
                Response::Listing(listing) => Ok(listing),
                Response::Error { message } => Err(message),
                other => Err(unexpected(&other)),
            };
        }
        let found = self.locate(path)?;
        Ok(Listing::of(
            &found.scan.root,
            found.scan.tree.root().bytes,
            &found.path,
            found.node,
            limit,
        ))
    }

    /// Estimate without adopting or handing over marks from either side.
    pub fn estimate_paths(
        &self,
        params: &EstimateParams,
    ) -> Result<String, String> {
        use disktree_core::planning::{self, Context};
        if params.paths.is_empty() || params.paths.len() > planning::MAX_PATHS {
            return Err(format!("Supply 1 to {} paths.", planning::MAX_PATHS));
        }
        let paths = params
            .paths
            .iter()
            .map(|p| self.absolute(p))
            .collect::<Result<Vec<_>, _>>()?;
        let peer_scope = params
            .peer_scope
            .as_ref()
            .map(|p| self.absolute(p))
            .transpose()?;
        let peer_limit =
            params.peer_limit.unwrap_or(10).min(planning::MAX_PEERS);
        // A normal connection hands over marks and Hello clears an intent.
        // This independent State/Estimate exchange does neither and never
        // adopts window marks or falls back to stale local trees on failure.
        let report = if let Ok(mut link) = Link::connect_at(&self.socket) {
            let state =
                match link.call(&Request::State).map_err(|e| e.to_string())? {
                    Response::State(state) => state,
                    Response::Error { message } => return Err(message),
                    other => return Err(unexpected(&other)),
                };
            if state.version != VERSION {
                return Err(disktree_link::protocol_mismatch(
                    state.version,
                    VERSION,
                ));
            }
            if let Some(host) = state.remote {
                return Err(disktree_link::remote_refusal(&host));
            }
            match link
                .call(&Request::Estimate {
                    paths,
                    peer_scope,
                    peer_limit,
                })
                .map_err(|e| e.to_string())?
            {
                Response::Estimate(report) => report,
                Response::Error { message } => return Err(message),
                other => return Err(unexpected(&other)),
            }
        } else {
            if self.link.is_some() || self.window.is_some() {
                return Err("The linked window is unavailable; reconnect before estimating its tree.".into());
            }
            let scan = self
                .scans
                .iter()
                .rev()
                .find(|scan| {
                    paths.iter().all(|path| path.starts_with(&scan.root))
                })
                .or_else(|| {
                    self.scans
                        .iter()
                        .rev()
                        .find(|scan| paths[0].starts_with(&scan.root))
                })
                .or_else(|| self.scans.last())
                .ok_or_else(|| {
                    "Nothing is scanned yet. Call pwrdisk_scan first."
                        .to_string()
                })?;
            planning::estimate(
                &scan.tree,
                Context::of(
                    &scan.root,
                    &scan.options,
                    scan.at,
                    scan.unreadable,
                ),
                &paths,
                peer_scope.as_deref(),
                peer_limit,
            )?
        };
        let json =
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        Ok(format!(
            "Read-only hypothetical selection; marks, scan root, navigation and removal intents are unchanged. All byte values are bytes.\n{json}\nPeer rows are alternatives, not additive savings. Additional bounds subtract the original selection's opposite bound. with_all_returned_peers evaluates their union once. Peers are clone-holder directories to inspect, not removal recommendations. Clone IDs cannot enumerate all extent owners, particularly after modification. references_not_seen_in_scan counts unresolved references without owner paths, never known peers; filtered or truncated scans cannot establish that count. Peer candidates are direct children of peers.scope; widen peer_scope explicitly to inspect another part of this scan."
        ))
    }

    /// List a directory, largest first.
    pub fn ls(&mut self, params: &LsParams) -> Result<String, String> {
        let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let now = now();
        let path = self.absolute(&params.path)?;
        let listing = self.listing(&path, limit)?;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "{}  {}",
            self.show(&listing.path),
            describe(&listing.entry, now)
        );
        if listing.path != listing.root {
            let _ = writeln!(
                out,
                "{:.0}% of {}, which is {}",
                share(listing.entry.bytes, listing.root_bytes),
                self.show(&listing.root),
                human_bytes(listing.root_bytes)
            );
        }
        if let Some(line) = clone_summary(&listing.entry) {
            out.push_str(&line);
        }
        out.push_str(&children(&listing, now));
        if params.show {
            out.push_str("Window: not shown. Listings never move the view; use pwrdisk_show only when the user asks to see a path.\n");
        }
        Ok(out)
    }

    /// What a search finds, from the window or from the scans here.
    fn found(&mut self, request: &FindRequest) -> Result<FoundList, String> {
        if self.try_connect() {
            if let Some(path) = &request.path {
                self.window_root_for(path)?;
            }
            return match self.call(&Request::Find(request.clone()))? {
                Response::Found(found) => Ok(found),
                Response::Error { message } => Err(message),
                other => Err(unexpected(&other)),
            };
        }
        let filter = request.filter()?;
        let scope: Vec<(PathBuf, Node<'_>)> = if let Some(path) = &request.path
        {
            let located = self.locate(path)?;
            vec![(located.path, located.node)]
        } else {
            if self.scans.is_empty() {
                return Err("Nothing is scanned yet. Call pwrdisk_scan first."
                    .to_string());
            }
            // A scan inside another one holds the same entries again; the
            // outer scan already counts them.
            self.scans
                .iter()
                .filter(|scan| {
                    !self.scans.iter().any(|outer| {
                        outer.root != scan.root
                            && scan.root.starts_with(&outer.root)
                    })
                })
                .map(|scan| (scan.root.clone(), scan.tree.root()))
                .collect()
        };
        let mut all = FoundList::default();
        for (path, node) in &scope {
            let found = FoundList::of(&query::find(
                path,
                *node,
                &filter,
                request.limit,
            ));
            all.matches.extend(found.matches);
            all.count += found.count;
            all.bytes += found.bytes;
        }
        all.matches
            .sort_by_key(|found| std::cmp::Reverse(found.entry.bytes));
        all.matches.truncate(request.limit);
        Ok(all)
    }

    /// The largest entries matching the filters, topmost matches only.
    pub fn find(&mut self, params: &FindParams) -> Result<String, String> {
        let now = now();
        let path = match &params.path {
            Some(path) => Some(self.absolute(path)?),
            None => None,
        };
        let request = params.request(path.clone(), now);
        let found = self.found(&request)?;
        let where_ = match (&path, &self.window) {
            (Some(path), _) => self.show(path),
            (None, Some(state)) if self.link.is_some() => {
                self.show(&state.root)
            }
            (None, _) => "the scanned roots".to_string(),
        };
        let mut out = String::new();
        if found.count == 0 {
            let _ = writeln!(out, "Nothing under {where_} matches.");
            return Ok(out);
        }
        let _ = writeln!(
            out,
            "{} match{} under {where_}, {} in all; the largest {}:",
            found.count,
            if found.count == 1 { "" } else { "es" },
            human_bytes(found.bytes),
            found.matches.len(),
        );
        out.push_str(&matches(
            &found.matches,
            found.bytes,
            now,
            self.home.as_deref(),
        ));
        if params.reclaimable
            || found
                .matches
                .iter()
                .any(|item| item.entry.reclaim.is_some())
        {
            out.push_str(
                "Cleanup candidates are classifications, not permission to \
                 remove. Totals are measured bytes, not guaranteed freed \
                 space. Prefer the owning tool's cleanup command for \
                 tool-owned data.\n",
            );
            out.push_str(&self.candidate_guards(&found));
        }
        Ok(out)
    }

    /// Preview the same guards as review, without adding any marks. Keep
    /// system caches visible, but explain why they cannot be removed.
    fn candidate_guards(&self, found: &FoundList) -> String {
        let targets: Vec<Target> = found
            .matches
            .iter()
            .filter(|item| item.entry.reclaim.is_some())
            .map(|item| Target {
                path: item.path.clone(),
                bytes: item.entry.bytes,
                is_dir: item.entry.is_dir,
                hidden: is_hidden(&item.path),
            })
            .collect();
        let mut out = String::new();
        for root in self.roots_of(&targets) {
            let mine: Vec<Target> = targets
                .iter()
                .filter(|target| target.path.starts_with(&root))
                .cloned()
                .collect();
            for blocked in plan(&mine, &root).blocked {
                let _ = writeln!(
                    out,
                    "Removal blocked: {} — {}",
                    self.show(&blocked.path),
                    blocked.reason
                );
            }
        }
        out.push_str(
            "Removal guards were checked for listed candidates only; \
             marking and review recheck eligibility.\n",
        );
        out
    }

    // ── the window ──────────────────────────────────────────────────────

    /// Whether the window link could be opened without launching anything
    /// or changing its root.
    /// A new link learns the window's state and is handed the marks made
    /// here before it existed; from then on the window's marks are the
    /// marks.
    fn try_connect(&mut self) -> bool {
        self.try_connect_for_request(None).unwrap_or(false)
    }

    /// Only an explicit scan may move a newly linked window to cover the
    /// marks made here. Check every mark before sending any of them. An
    /// explicit unmark reaches the window even if the handover still fails.
    fn try_connect_for_request(
        &mut self,
        request: Option<&Request>,
    ) -> Result<bool, String> {
        if self.link.is_some() {
            if let Some(request) = request {
                self.send_state_request(request)?;
            }
            return Ok(true);
        }
        let Ok(link) = Link::connect_at(&self.socket) else {
            self.unlinked = None;
            return Ok(false);
        };
        self.link = Some(link);
        // `hello` adopts the window's marks, which would lose these.
        let mine = std::mem::take(&mut self.marks);
        let hello = Request::Hello {
            client: self.client.clone(),
            version: VERSION,
        };
        let mut unmarked = false;
        let refused = match self.call(&hello) {
            // A window on an SSH host answers, and `adopt` noted why it is
            // not followed.
            Ok(Response::State(_)) if self.unlinked.is_some() => None,
            Ok(Response::State(state)) => match (|| {
                // A failed handover can leave accepted marks in the
                // window. Clear them before retrying the remaining marks,
                // so a refusal cannot resurrect an explicitly unmarked path.
                if let Some(request @ Request::Unmark { .. }) = request {
                    self.send_state_request(request)?;
                    unmarked = true;
                }
                let root = match request {
                    Some(Request::Scan { root, .. }) => root,
                    _ => &state.root,
                };
                Self::check_handover_root(&mine, root)?;
                if let Some(request @ Request::Scan { .. }) = request {
                    self.send_state_request(request)?;
                }
                self.push_marks(&mine)
            })() {
                Ok(()) => return Ok(true),
                Err(error) => Some(format!(
                    "The open PwrDisk window did not take the marks made \
                     before it was linked ({error}); they are kept here."
                )),
            },
            // A version mismatch, said in the window's words.
            Ok(Response::Error { message }) => {
                Some(if message.contains(disktree_link::RECONNECT_MCP) {
                    message
                } else {
                    format!("{message} {}", disktree_link::RECONNECT_MCP)
                })
            }
            Ok(other) => Some(unexpected(&other)),
            Err(reason) => {
                Some(format!("{reason}. {}", disktree_link::RECONNECT_MCP))
            }
        };
        let request_refused =
            request.is_some() && refused.is_some() && !unmarked;
        if refused.is_some() {
            self.unlinked = refused;
        }
        self.link = None;
        self.window = None;
        self.marks = mine;
        if request_refused && let Some(reason) = &self.unlinked {
            return Err(reason.clone());
        }
        Ok(false)
    }

    fn send_state_request(&mut self, request: &Request) -> Result<(), String> {
        match self.call(request)? {
            Response::State(_) => Ok(()),
            Response::Error { message } => Err(message),
            other => Err(unexpected(&other)),
        }
    }

    fn check_handover_root(mine: &[Target], root: &Path) -> Result<(), String> {
        if !mine.is_empty()
            && (root.as_os_str().is_empty()
                || mine.iter().any(|mark| {
                    mark.path == root || !mark.path.starts_with(root)
                }))
        {
            return Err(
                "the window's root does not contain all the marks. Call \
                 pwrdisk_scan on a directory containing all marked paths \
                 to move the window explicitly and hand them over"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Open the link, launching a window on `root` when none answers.
    fn ensure_link(&mut self, root: &Path) -> Result<(), String> {
        if self.try_connect() {
            return Ok(());
        }
        if let Some(reason) = &self.unlinked {
            return Err(reason.clone());
        }
        if !disktree_link::SUPPORTED {
            return Err(NO_LINK.to_string());
        }
        let Some(launcher) = &self.launcher else {
            return Err(format!(
                "No PwrDisk window is open, and this session cannot open \
                 one here ({}). Start `pwrdisk {}` yourself, then call \
                 again.",
                Link::connect_at(&self.socket)
                    .err()
                    .map_or_else(String::new, |error| error.to_string()),
                root.display()
            ));
        };
        if let Some(launched) = self.launched
            && launched.elapsed() < RELAUNCH_AFTER
        {
            return Err(format!(
                "A PwrDisk window was started {} s ago and has not \
                 answered yet. Give it a moment and call again; if none \
                 appeared, start `pwrdisk {}` by hand.",
                launched.elapsed().as_secs(),
                root.display()
            ));
        }
        launcher(root).map_err(|error| {
            format!("could not start a PwrDisk window: {error}")
        })?;
        let started = Instant::now();
        self.launched = Some(started);
        while started.elapsed() < LAUNCH_WAIT {
            std::thread::sleep(Duration::from_millis(100));
            if self.try_connect() {
                self.launched = None;
                return Ok(());
            }
        }
        Err("A PwrDisk window was started but did not answer in time. \
             Call again once it is open."
            .to_string())
    }

    /// Send one request. Every answer that carries the window's state is
    /// adopted. A failed link is dropped, so the next call tries again
    /// from the start.
    fn call(&mut self, request: &Request) -> Result<Response, String> {
        let Some(link) = &mut self.link else {
            return Err("the window is not linked".to_string());
        };
        match link.call(request) {
            Ok(response) => {
                if let Response::State(state) | Response::Marked { state, .. } =
                    &response
                {
                    // Validate both ends, even if a stale peer accepted Hello.
                    // Never adopt its marks or interpret missing savings as
                    // measured allocation before establishing compatibility.
                    if state.version != VERSION {
                        let reason = disktree_link::protocol_mismatch(
                            state.version,
                            VERSION,
                        );
                        self.unlinked = Some(reason.clone());
                        self.link = None;
                        self.window = None;
                        return Ok(Response::error(reason));
                    }
                    self.adopt(state);
                }
                Ok(response)
            }
            Err(error) => {
                self.link = None;
                self.window = None;
                Err(format!("the PwrDisk window link failed: {error}"))
            }
        }
    }

    /// The window's marks are the marks: keep ours in step. A window on an
    /// SSH host is only noted, and the link to it dropped: its marks are
    /// another machine's paths.
    fn adopt(&mut self, state: &State) {
        if let Some(host) = &state.remote {
            self.unlinked = Some(disktree_link::remote_refusal(host));
            self.window = None;
            self.link = None;
            return;
        }
        self.unlinked = None;
        self.marks = state
            .marks
            .iter()
            .map(|mark| Target {
                hidden: is_hidden(&mark.path),
                path: mark.path.clone(),
                bytes: mark.bytes,
                is_dir: mark.is_dir,
            })
            .collect();
        self.window = Some(state.clone());
    }

    /// Hand marks made before the window linked over to it. Each answer
    /// carries the window's marks, which replace ours.
    fn push_marks(&mut self, mine: &[Target]) -> Result<(), String> {
        if !mine.is_empty() {
            let root = self
                .window
                .as_ref()
                .ok_or("the window has not said where it is")?
                .root
                .clone();
            Self::check_handover_root(mine, &root)?;
            let targets: Vec<MarkRequest> = mine
                .iter()
                .map(|target| MarkRequest {
                    path: target.path.clone(),
                    bytes: target.bytes,
                    is_dir: target.is_dir,
                })
                .collect();
            match self.call(&Request::Mark { root, targets })? {
                Response::Marked { refused, .. } if !refused.is_empty() => {
                    return Err(refused
                        .iter()
                        .map(|item| {
                            format!(
                                "{}: {}",
                                self.show(&item.path),
                                item.reason
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("; "));
                }
                Response::Marked { .. } | Response::State(_) => {}
                Response::Error { message } => return Err(message),
                other => return Err(unexpected(&other)),
            }
        }
        Ok(())
    }

    /// Show `path` in the window, opening one on `root` if needed.
    fn show_in_window(
        &mut self,
        root: &Path,
        path: &Path,
    ) -> Result<String, String> {
        let path = self.absolute(path)?;
        self.ensure_link(root)?;
        match self.call(&Request::Show {
            root: root.to_path_buf(),
            path: path.clone(),
        })? {
            Response::Error { message } => Err(message),
            _ => Ok(format!("Window: showing {}.\n", self.show(&path))),
        }
    }

    pub fn show_path(&mut self, params: &ShowParams) -> Result<String, String> {
        let path = self.absolute(&params.path)?;
        let root = if self.try_connect() {
            // A path outside the window but inside a scan held here moves
            // the window to that scan's root; one in neither is an error.
            match self.window_root_for(&path) {
                Ok(root) => root,
                Err(outside) => self
                    .locate(&path)
                    .map(|found| found.scan.root.clone())
                    .map_err(|_| outside)?,
            }
        } else {
            self.locate(&path)?.scan.root.clone()
        };
        self.show_in_window(&root, &path)
    }

    /// What the window is doing, in one line, for the end of a report.
    fn window_line(&mut self) -> String {
        // A link that was already open may be stale; ask again.
        if self.try_connect() {
            let _ = self.call(&Request::State);
        }
        if let Some(reason) = &self.unlinked {
            return format!("Window: open, but not followed. {reason}");
        }
        match &self.window {
            Some(state) => {
                let tree = match &state.tree {
                    Some(tree) => format!(
                        " ({} \u{00b7} {} files \u{00b7} scanned {} \u{00b7} {})",
                        human_bytes(tree.bytes),
                        human_count(tree.files),
                        age(now(), tree.scanned_at),
                        scan_context(tree),
                    ),
                    None => String::new(),
                };
                format!(
                    "Window: open on {}{tree}, {} screen{}{} (PwrDisk {}). Its tree is \
                     what pwrdisk_ls and pwrdisk_find read.\n{}{}",
                    self.show(&state.root),
                    state.screen,
                    if state.scanning { ", scanning" } else { "" },
                    match state.marks.len() {
                        0 => String::new(),
                        1 => ", 1 mark".to_string(),
                        n => format!(", {n} marks"),
                    },
                    if state.app_version.is_empty() {
                        "version unknown"
                    } else {
                        &state.app_version
                    },
                    version_notice(state),
                    if state.tree.is_some() {
                        allocation_caveat()
                    } else {
                        ""
                    },
                )
            }
            None if self.launcher.is_some() => {
                "Window: none open. pwrdisk_review opens one on the marks \
                 for the user to confirm; pwrdisk_show opens one on a path."
                    .to_string()
            }
            None if !disktree_link::SUPPORTED => {
                "Window: no link on this system yet. The marks stay in this \
                 session; pwrdisk_review says how the user takes them over."
                    .to_string()
            }
            None => "Window: none open, and none can be opened from here."
                .to_string(),
        }
    }

    /// A read-only preview does not navigate, change marks, or start removal.
    pub fn removal_preview(
        &mut self,
        params: &RemovalPreviewParams,
    ) -> Result<String, String> {
        self.pending_removal = None;
        if !self.try_connect() {
            return Err("An open, linked PwrDisk window with an idle local scan is required. No headless removal is available.".into());
        }
        match self.call(&Request::PrepareRemoval {
            mode: params.mode.into(),
        })? {
            Response::RemovalPreview(preview) => {
                self.pending_removal = Some(preview.clone());
                let json = serde_json::to_string_pretty(&preview)
                    .map_err(|error| error.to_string())?;
                Ok(format!(
                    "{}\n{}{json}\nShow the user every path, the mode, listed bytes, estimate bounds and capacity limit. Wait for explicit confirmation of this operation before pwrdisk_remove. Approval to mark is insufficient. Never approve through computer use. Do not split the selection into batches to evade the limit. Confirmation text: {}. A changed or expired preview requires new user confirmation.",
                    preview.mode.warning(),
                    estimate_report(&preview.estimate),
                    preview.mode.confirmation()
                ))
            }
            Response::Error { message } => Err(message),
            other => Err(unexpected(&other)),
        }
    }

    /// Consume the session copy before asking the client for human input.
    /// The app copy is independently checked after the client answers.
    pub fn removal_confirmation_message(
        &mut self,
        params: &RemoveParams,
    ) -> Result<String, String> {
        let preview = self.pending_removal.take().ok_or(
            "Obtain a new removal preview before asking for confirmation",
        )?;
        let mode: RemovalChoice = params.mode.into();
        if params.preview_id != preview.preview_id
            || params.paths != preview.paths
            || mode != preview.mode
            || params.confirmation != mode.confirmation()
        {
            return Err("The removal request differs from the preview; obtain a new preview and user confirmation".into());
        }
        let mut message = format!(
            "{}\n\nConfirm removal of {} listed path(s), measured {}.\nCapacity limit: {}% of each volume.\n\nPaths:\n",
            mode.warning(),
            preview.paths.len(),
            human_bytes(preview.bytes),
            preview.limit_percent,
        );
        for path in &preview.paths {
            // Keep Windows separators readable while exposing control
            // characters that could otherwise disguise the path list.
            let shown: String = path
                .to_string_lossy()
                .chars()
                .map(|character| {
                    if character.is_control() {
                        character.escape_default().to_string()
                    } else {
                        character.to_string()
                    }
                })
                .collect();
            let _ = writeln!(message, "  {shown}");
        }
        for volume in &preview.volumes {
            let _ = writeln!(
                message,
                "Volume {}: {} selected; limit {} of {} capacity.",
                volume.volume,
                human_bytes(volume.bytes),
                human_bytes(volume.limit),
                human_bytes(volume.capacity)
            );
        }
        message.push_str(&estimate_report(&preview.estimate));
        message.push_str("\nApproval to mark does not authorize removal. Only these listed paths are included; directory removal includes their contents. Listed bytes are not guaranteed freed space.");
        Ok(message)
    }

    /// A destructive write. The app consumes and validates the preview again.
    pub fn remove(&mut self, params: &RemoveParams) -> Result<String, String> {
        let mode: RemovalChoice = params.mode.into();
        if params.confirmation != mode.confirmation() {
            return Err("Explicit confirmation of the preview's paths, mode and consequences is required; never approve with computer use".into());
        }
        if !self.try_connect() {
            return Err("The window is not linked. Do not retry removal; obtain a fresh preview and user confirmation.".into());
        }
        match self.call(&Request::Remove(ConfirmedRemoval {
            preview_id: params.preview_id.clone(),
            mode,
            paths: params.paths.clone(),
            confirmation: params.confirmation.clone(),
            rescan: params.rescan,
        }))? {
            Response::State(_) => Ok(format!(
                "{}\nConfirmed removal started. pwrdisk_status reports completion and failures. Never repeat this call automatically.",
                mode.warning()
            )),
            Response::Error { message } => Err(message),
            other => Err(unexpected(&other)),
        }
    }

    // ── marking ─────────────────────────────────────────────────────────

    /// Mark paths for removal, in the window when one is open.
    pub fn mark(&mut self, params: &PathsParams) -> Result<String, String> {
        if params.paths.is_empty() {
            return Err("give at least one path to mark".to_string());
        }
        let mut paths = Vec::new();
        for path in &params.paths {
            paths.push(self.absolute(path)?);
        }
        let refused = if self.try_connect() {
            self.mark_in_window(&paths)?
        } else {
            self.mark_here(&paths)?
        };
        let mut report = String::new();
        if !refused.is_empty() {
            let _ = writeln!(report, "Not marked:");
            for reason in &refused {
                let _ = writeln!(report, "  {reason}");
            }
        }
        report.push_str(&self.plan_report());
        Ok(report)
    }

    /// Mark in the window, which measures each path from its own tree.
    /// Returns what was refused, or an error when nothing could be marked.
    fn mark_in_window(
        &mut self,
        paths: &[PathBuf],
    ) -> Result<Vec<String>, String> {
        let mut refused = Vec::new();
        let mut targets = Vec::new();
        let mut root = None;
        for path in paths {
            let window_root = match self.window_root_for(path) {
                Ok(root) => root,
                Err(reason) => {
                    refused.push(reason);
                    continue;
                }
            };
            if *path == window_root {
                refused.push(format!(
                    "{} is the scanned root; mark what is inside it instead",
                    self.show(path)
                ));
                continue;
            }
            // A scan held here may know the size already; the window
            // measures regardless.
            let (bytes, is_dir) = self
                .locate(path)
                .map_or((0, false), |f| (f.node.bytes, f.node.is_dir()));
            targets.push(MarkRequest {
                path: path.clone(),
                bytes,
                is_dir,
            });
            root = Some(window_root);
        }
        let Some(root) = root else {
            return Err(refused.join("\n"));
        };
        match self.call(&Request::Mark { root, targets })? {
            Response::Marked { refused: more, .. } => {
                for item in more {
                    // The window's reason usually names the path already.
                    let shown = self.show(&item.path);
                    refused.push(if item.reason.contains(&shown) {
                        item.reason
                    } else {
                        format!("{shown}: {}", item.reason)
                    });
                }
            }
            Response::State(_) => {}
            Response::Error { message } => return Err(message),
            other => return Err(unexpected(&other)),
        }
        if self.marks.is_empty() && !refused.is_empty() {
            return Err(refused.join("\n"));
        }
        Ok(refused)
    }

    /// Mark against this process's own scans, with no window to ask.
    fn mark_here(&mut self, paths: &[PathBuf]) -> Result<Vec<String>, String> {
        let mut refused = Vec::new();
        let mut targets = Vec::new();
        for path in paths {
            let found = match self.locate(path) {
                Ok(found) => found,
                Err(reason) => {
                    refused.push(reason);
                    continue;
                }
            };
            if found.crumbs.is_empty() {
                refused.push(format!(
                    "{} is the scanned root; mark what is inside it instead",
                    self.show(&found.path)
                ));
                continue;
            }
            targets.push(Target {
                hidden: is_hidden(&found.path),
                is_dir: found.node.is_dir(),
                bytes: found.node.bytes,
                path: found.path,
            });
        }
        if targets.is_empty() {
            return Err(refused.join("\n"));
        }
        for target in targets {
            if !self.marks.iter().any(|known| known.path == target.path) {
                self.marks.push(target);
            }
        }
        Ok(refused)
    }

    /// Unmark paths, or everything.
    pub fn unmark(&mut self, params: &PathsParams) -> Result<String, String> {
        let mut paths = Vec::new();
        for path in &params.paths {
            paths.push(self.absolute(path)?);
        }
        let previous = self.marks.clone();
        if paths.is_empty() {
            self.marks.clear();
        } else {
            self.marks.retain(|target| !paths.contains(&target.path));
        }
        if let Err(reason) =
            self.try_connect_for_request(Some(&Request::Unmark { paths }))
        {
            self.marks = previous;
            return Err(reason);
        }
        Ok(self.plan_report())
    }

    /// The scan roots the marks fall under, each once: the window's root
    /// when it covers a mark, else a scan's here, else the mark's parent.
    fn roots_of(&self, marks: &[Target]) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = Vec::new();
        for mark in marks {
            let root = self
                .window
                .as_ref()
                .filter(|state| mark.path.starts_with(&state.root))
                .map(|state| state.root.clone())
                .or_else(|| {
                    self.scans
                        .iter()
                        .filter(|scan| mark.path.starts_with(&scan.root))
                        .max_by_key(|scan| scan.root.as_os_str().len())
                        .map(|scan| scan.root.clone())
                })
                .unwrap_or_else(|| {
                    mark.path
                        .parent()
                        .map_or_else(|| mark.path.clone(), Path::to_path_buf)
                });
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        roots
    }

    /// What is marked, what the guards keep back, and what it frees.
    pub fn plan_report(&mut self) -> String {
        // Linking can replace our marks; describe the same state as the
        // window line, including after an explicit unmark.
        let window = self.window_line();
        let mut out = String::new();
        if self.marks.is_empty() {
            out.push_str("Nothing is marked.\n");
            out.push_str(&window);
            return out;
        }
        let total: u64 = self.marks.iter().map(|mark| mark.bytes).sum();
        let _ = writeln!(
            out,
            "Marked {} \u{00b7} {}",
            self.marks.len(),
            human_bytes(total)
        );
        let marks = self.marks.clone();
        let mut plans = Vec::new();
        for root in self.roots_of(&marks) {
            let mine: Vec<Target> = marks
                .iter()
                .filter(|mark| mark.path.starts_with(&root))
                .cloned()
                .collect();
            plans.push(plan(&mine, &root));
        }
        if self.window.is_none() {
            plans = union_reports(plans);
        }
        for plan in plans {
            out.push_str(&self.report_plan(plan));
        }
        out.push_str(&window);
        out
    }

    /// Bounds from the tree that owns this accepted selection.
    fn selection_estimate(&self, plan: &mut Plan) -> SelectionEstimate {
        if let Some(window) = &self.window {
            // Only the window owns the linked tree. Never substitute an old
            // local scan or add per-mark estimates across clone groups.
            return window
                .estimate
                .as_ref()
                .filter(|estimate| {
                    window.root == plan.root
                        && !window.scanning
                        && window.scanned
                        && estimate.paths
                            == plan
                                .targets
                                .iter()
                                .map(|t| t.path.clone())
                                .collect::<Vec<_>>()
                })
                .cloned()
                .unwrap_or_else(|| SelectionEstimate::unknown(plan));
        }
        let mut unmeasured = false;
        let mut selected: Option<Vec<Node<'_>>> = None;
        plan.estimate_selection(|targets| {
            let mut nodes = Vec::with_capacity(targets.len());
            let mut unknown = Estimate::default();
            for target in targets {
                if let Ok(found) = self.locate(&target.path) {
                    if found.scan.options.apparent_size
                        || (cfg!(target_os = "macos")
                            && !found.scan.options.apfs_clone_metadata)
                    {
                        unmeasured = true;
                        unknown.upper =
                            unknown.upper.saturating_add(target.bytes);
                        unknown.partial = true;
                    } else {
                        nodes.push(found.node);
                        unknown.partial |= found.scan.unreadable > 0
                            || found.scan.options.max_depth.is_some();
                    }
                } else {
                    unknown.upper = unknown.upper.saturating_add(target.bytes);
                    unknown.partial = true;
                }
            }
            // The first call covers every target: keep its nodes for the
            // shared total instead of locating each mark again.
            if selected.is_none() {
                selected = Some(nodes.clone());
            }
            let measured = sharing::estimate(nodes);
            Estimate {
                lower: measured.lower,
                upper: measured.upper.saturating_add(unknown.upper),
                partial: measured.partial || unknown.partial,
            }
        });
        let mut estimate = SelectionEstimate::of(plan)
            .with_shared(selected.unwrap_or_default());
        if unmeasured {
            estimate.upper = None;
        }
        estimate
    }

    /// Accepted targets, covered marks, guard refusals and savings bounds.
    fn report_plan(&self, mut plan: Plan) -> String {
        let mut out = String::new();
        let root = plan.root.clone();
        let estimate = self.selection_estimate(&mut plan);
        for target in &plan.targets {
            let _ = writeln!(
                out,
                "  {:>9}  {}{}",
                human_bytes(target.bytes),
                self.show(&target.path),
                if target.is_dir { "/" } else { "" },
            );
        }
        if !plan.covered.is_empty() {
            let _ = writeln!(
                out,
                "Covered by another mark ({}): {}",
                plan.covered.len(),
                plan.covered
                    .iter()
                    .map(|target| self.show(&target.path))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if !plan.blocked.is_empty() {
            let _ = writeln!(out, "Kept back ({}):", plan.blocked.len());
            for blocked in &plan.blocked {
                let _ = writeln!(
                    out,
                    "  {}  {}",
                    self.show(&blocked.path),
                    blocked.reason
                );
            }
        }
        out.push_str(&estimate_report(&estimate));
        match space_info(&root).ok() {
            Some(space) => {
                let after = space.after_removing(estimate.on_volume);
                let _ = writeln!(
                    out,
                    "Projects {} on {}: {} free now, {} after.",
                    human_bytes(estimate.on_volume),
                    device_for(&root).unwrap_or_else(|| self.show(&root)),
                    human_bytes(space.available),
                    human_bytes(after.available),
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "Projects {} on the volume of {}.",
                    human_bytes(estimate.on_volume),
                    self.show(&root)
                );
            }
        }
        if plan.foreign() > 0 {
            let _ = writeln!(
                out,
                "Listed {} on other volumes, excluded from this projection.",
                human_bytes(plan.foreign())
            );
        }
        if plan.unattributed() > 0 {
            let _ = writeln!(
                out,
                "{} whose volume cannot be told is not counted either.",
                human_bytes(plan.unattributed())
            );
        }
        out
    }

    /// Open the review screen in the window, launching one if needed.
    pub fn review(&mut self) -> Result<String, String> {
        if self.marks.is_empty() {
            return Err(
                "Nothing is marked. pwrdisk_mark some paths first.".to_string()
            );
        }
        let marks = self.marks.clone();
        let roots = self.roots_of(&marks);
        let mut roots = roots.iter();
        let mut root = roots.next().cloned().ok_or("no root for the marks")?;
        // A new window must cover every offline scan root, rather than
        // starting on one and immediately refusing the other marks.
        for other in roots {
            while !other.starts_with(&root) {
                if !root.pop() {
                    return Err(
                        "The marked paths have no common scan root. Review \
                         marks on one drive at a time."
                            .to_string(),
                    );
                }
            }
        }
        self.ensure_link(&root)?;
        if let Response::Error { message } = self.call(&Request::Review)? {
            return Err(message);
        }
        let _ = self.call(&Request::State);
        // The window starts in the trash whenever this machine has one, and
        // otherwise deletes; the agent has to say which before the user
        // presses Enter.
        let backend = detect_trash_backend();
        let how = if backend.is_available() {
            format!(
                "Enter moves the marks to {}, where they can be recovered \
                 and their space comes back only once it is emptied; \
                 permanent deletion is a choice away and always asks first",
                backend.label()
            )
        } else {
            "This system has no trash, so Enter deletes the marks \
             permanently; tell the user that before they confirm"
                .to_string()
        };
        Ok(format!(
            "The review screen is open in PwrDisk: {} marked. \
             The user decides there. {how}. Afterwards pwrdisk_status \
             reports what was actually freed.\n{}",
            self.marks.len(),
            self.plan_report()
        ))
    }

    // ── status ──────────────────────────────────────────────────────────

    /// Everything this session knows, in a screenful.
    pub fn status(&mut self) -> String {
        let now = now();
        let mut out = String::new();
        let _ = writeln!(
            out,
            "PwrDisk {} MCP server serving {}. Every removal requires user \
             confirmation of the exact paths and mode; never approve \
             through computer use.",
            env!("CARGO_PKG_VERSION"),
            self.client
        );
        out.push_str(&system_line());
        out.push_str(&self.window_line());
        out.push('\n');
        if self.scans.is_empty() {
            if self.link.is_none() {
                out.push_str(
                    "Scans: none yet. pwrdisk_scan with no arguments scans \
                     the home directory in seconds; disk: true scans the \
                     volume.\n",
                );
            }
        } else {
            out.push_str(if self.link.is_some() {
                "Scans held here, unused while the window is open:\n"
            } else {
                "Scans:\n"
            });
            for scan in &self.scans {
                let _ = writeln!(
                    out,
                    "  {}  {} \u{00b7} {} files \u{00b7} {} \u{00b7} took \
                     {:.1} s",
                    self.show(&scan.root),
                    human_bytes(scan.tree.root().bytes),
                    human_count(scan.tree.root().files),
                    age(now, scan.at),
                    scan.elapsed.as_secs_f64(),
                );
            }
        }
        if let Some(removal) =
            self.window.as_ref().and_then(|state| state.removal.clone())
        {
            let _ = writeln!(
                out,
                "Last removal: {} \u{00b7} {} removed \u{00b7} {} failed \
                 \u{00b7} {} marked \u{00b7} {} \u{00b7} {}",
                removal.mode,
                removal.removed,
                removal.failed,
                human_bytes(removal.bytes),
                removal.freed.map_or_else(
                    || "freed space not measured".to_string(),
                    |freed| format!(
                        "{} freed on the volume",
                        human_bytes(freed)
                    )
                ),
                age(now, removal.at),
            );
        }
        if let Some(ledger) = self
            .window
            .as_ref()
            .and_then(|state| state.removals.as_ref())
        {
            out.push_str(&crate::report::ledger(ledger, now));
        }
        if self.marks.is_empty() {
            out.push_str("Marked: nothing.\n");
        } else {
            let total: u64 = self.marks.iter().map(|mark| mark.bytes).sum();
            let _ = writeln!(
                out,
                "Marked: {} \u{00b7} {} (pwrdisk_mark with no new paths, or \
                 pwrdisk_unmark, shows the plan)",
                self.marks.len(),
                human_bytes(total)
            );
        }
        out.push_str(&volumes_report());
        out
    }
}

/// Guarded offline plans, combined for reporting only. Removal still needs a
/// window. Drive/UNC roots stay separate: neither clones nor hardlinks cross
/// those namespaces, and there is no common path to attribute their meter to.
fn union_reports(plans: Vec<Plan>) -> Vec<Plan> {
    let mut groups = std::collections::BTreeMap::<PathBuf, Vec<Plan>>::new();
    for plan in plans {
        let namespace = plan
            .root
            .ancestors()
            .last()
            .unwrap_or(&plan.root)
            .to_path_buf();
        groups.entry(namespace).or_default().push(plan);
    }
    groups
        .into_values()
        .map(|mut plans| {
            if plans.len() == 1 {
                return plans.pop().expect("one plan");
            }
            // Each path already passed guards against its own scanned root.
            let mut common = plans[0].root.clone();
            for plan in &plans[1..] {
                while !plan.root.starts_with(&common) && common.pop() {}
            }
            let targets: Vec<_> = plans
                .iter()
                .flat_map(|plan| plan.targets.iter().cloned())
                .collect();
            let mut union = plan(&targets, &common);
            for plan in plans {
                union.blocked.extend(plan.blocked);
                union.covered.extend(plan.covered);
            }
            union
        })
        .collect()
}

fn version_notice(window: &State) -> String {
    if window.app_version.is_empty() {
        format!(
            "GUI application version unavailable. {}\n",
            disktree_link::RECONNECT_MCP
        )
    } else if window.app_version != env!("CARGO_PKG_VERSION") {
        format!(
            "Release mismatch: MCP server {}; GUI {}. Link protocol {} is compatible, so the connection remains usable. {}\n",
            env!("CARGO_PKG_VERSION"),
            window.app_version,
            VERSION,
            disktree_link::RECONNECT_MCP,
        )
    } else {
        String::new()
    }
}

fn estimate_report(estimate: &SelectionEstimate) -> String {
    let mut out = format!(
        "Selected-set reclaim estimate: {}–{}{}. Before snapshots, open files \
         and changes since the scan; actual freed space can be lower.\n",
        human_bytes(estimate.lower),
        estimate
            .upper
            .map_or_else(|| "unknown upper bound".into(), human_bytes),
        if estimate.partial {
            " (incomplete scan or metadata)"
        } else {
            ""
        },
    );
    // APFS reference counts include clones outside the scan, which can
    // never be selected: say why the estimate is below the listed bytes.
    if estimate.shared > 0 {
        let _ = writeln!(
            out,
            "{} of the selection is APFS clone blocks. They count only when \
             every clone of them is selected; a clone left unselected, or \
             outside this scan, keeps them.",
            human_bytes(estimate.shared)
        );
    }
    out
}

/// Every volume a person would call a disk, fullest first.
fn volumes_report() -> String {
    let mut out = String::new();
    let volumes: Vec<_> = volumes().into_iter().filter(worth_listing).collect();
    if volumes.is_empty() {
        return out;
    }
    out.push_str("Volumes, fullest first:\n");
    for volume in volumes.iter().take(8) {
        match volume.space {
            Some(space) => {
                let _ = writeln!(
                    out,
                    "  {}  {} free of {} ({:.0}% used){}",
                    volume.point.display(),
                    human_bytes(space.available),
                    human_bytes(space.total),
                    f64::from(space.used_fraction()) * 100.0,
                    volume
                        .device
                        .as_deref()
                        .map_or_else(String::new, |device| format!(
                            "  {device}"
                        )),
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "  {}  free space unknown",
                    volume.point.display()
                );
            }
        }
    }
    out
}

/// Whether a volume is one a person would call a disk: the system's own
/// slices, device files, simulator images and automount stubs are left
/// out, as is anything under a gigabyte.
fn worth_listing(volume: &disktree_core::space::Volume) -> bool {
    const GIB: u64 = 1024 * 1024 * 1024;
    let point = volume.point.to_string_lossy();
    let system = [
        "/System/Volumes/",
        "/private/",
        "/dev",
        "/Library/Developer/CoreSimulator/",
        "/proc",
        "/sys",
        "/run",
        "/boot/efi",
        "/snap/",
    ]
    .iter()
    .any(|prefix| point.starts_with(prefix));
    !system
        && volume
            .device
            .as_deref()
            .is_none_or(|device| !device.starts_with("map "))
        && volume.space.is_some_and(|space| space.total >= GIB)
}

/// Where a path landed in a scan held here.
struct Located<'a> {
    scan: &'a Scan,
    path: PathBuf,
    crumbs: Vec<usize>,
    node: Node<'a>,
}

/// What a scan report is made of, whichever side did the walking.
struct ScanFacts {
    root: PathBuf,
    tree: TreeSummary,
    space: Option<Space>,
    device: Option<String>,
    top: Listing,
    insights: Vec<Insight>,
}

impl ScanFacts {
    fn of_local(scan: &Scan) -> Self {
        Self {
            root: scan.root.clone(),
            tree: TreeSummary {
                bytes: scan.tree.root().bytes,
                files: scan.tree.root().files,
                dirs: u64::from(scan.tree.root().dirs),
                scanned_at: scan.at,
                elapsed_ms: u64::try_from(scan.elapsed.as_millis())
                    .unwrap_or(u64::MAX),
                unreadable: scan.unreadable,
                apparent_size: scan.options.apparent_size,
                include_hidden: scan.options.include_hidden,
            },
            space: scan.space.map(|space| Space {
                total: space.total,
                free: space.free,
                available: space.available,
            }),
            device: scan.device.clone(),
            top: Listing::of(
                &scan.root,
                scan.tree.root().bytes,
                &scan.root,
                scan.tree.root(),
                TOP,
            ),
            insights: scan
                .insights
                .iter()
                .map(|candidate| Insight {
                    path: path_of(
                        &scan.root,
                        scan.tree.root(),
                        &candidate.crumbs,
                        disktree_core::path_style::PathStyle::Native,
                    ),
                    bytes: candidate.bytes,
                    what: candidate.finding.describe(),
                })
                .collect(),
        }
    }
}

/// A dotfile or dot-directory by name.
fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
}

/// An answer of a shape the request does not take: an old window.
fn unexpected(response: &Response) -> String {
    format!(
        "the window answered with something unexpected ({}). {}",
        match response {
            Response::State(_) => "state",
            Response::Marked { .. } => "marked",
            Response::Listing(_) => "listing",
            Response::Found(_) => "found",
            Response::Estimate(_) => "estimate",
            Response::RemovalPreview(_) => "removal_preview",
            Response::Ok => "ok",
            Response::Error { .. } => "error",
        },
        disktree_link::RECONNECT_MCP,
    )
}

/// Walk `root` with `options` and keep what is needed later. Blocking;
/// the server runs it off the protocol thread.
pub fn run_scan(root: PathBuf, options: ScanOptions) -> Result<Scan, String> {
    let started = Instant::now();
    let handle = ScanHandle::spawn(root.clone(), options.clone());
    let outcome = loop {
        if let Some(outcome) = handle.poll() {
            break outcome;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let tree = outcome.map_err(|error| {
        format!("scan of {} failed: {error}", root.display())
    })?;
    let snapshot = handle.progress.snapshot();
    let at = now();
    let insights = worth_a_look(tree.root(), at, INSIGHTS);
    Ok(Scan {
        space: space_info(&root).ok(),
        device: device_for(&root),
        root,
        tree: Arc::new(tree),
        options,
        at,
        elapsed: started.elapsed(),
        unreadable: snapshot.errors,
        insights,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use disktree_core::tree::Draft;
    #[cfg(any(unix, windows))]
    use disktree_link::{Incoming, Listener, Mark, Refused, Removal};
    #[cfg(any(unix, windows))]
    use std::sync::Mutex;
    #[cfg(any(unix, windows))]
    use std::sync::atomic::{AtomicBool, Ordering};
    #[cfg(any(unix, windows))]
    use std::thread;

    fn endpoint(dir: &Path) -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(format!(
                r"\\.\pipe\pwrdisk-test-{}",
                dir.file_name().expect("temp name").to_string_lossy()
            ))
        }
        #[cfg(not(windows))]
        {
            dir.join("window.sock")
        }
    }

    /// A small tree on disk: a cache, a build directory, a checkout with a
    /// large file, and a stray download.
    fn fixture() -> tempfile::TempDir {
        let temp = tempfile::TempDir::new().expect("tempdir");
        for (path, bytes) in [
            (".cache/pip/blob.bin", 400_000_usize),
            ("src/app/target/debug/app", 300_000),
            ("src/app/Cargo.toml", 100),
            ("src/app/main.rs", 2_000),
            ("Downloads/image.iso", 250_000),
            ("notes/todo.txt", 500),
        ] {
            let file = temp.path().join(path);
            std::fs::create_dir_all(file.parent().expect("parent"))
                .expect("mkdir");
            std::fs::write(&file, vec![b'x'; bytes]).expect("write");
        }
        temp
    }

    /// Apparent sizes, so the numbers are the bytes written, not blocks.
    #[cfg(any(unix, windows))]
    fn apparent() -> ScanOptions {
        ScanOptions {
            apparent_size: true,
            ..ScanOptions::default()
        }
    }

    /// Scan here, as the server does when no window is open.
    fn scanned(session: &mut Session, root: &Path) -> String {
        let plan = session
            .prepare_scan(&ScanParams {
                path: Some(root.to_path_buf()),
                apparent_size: true,
                ..ScanParams::default()
            })
            .expect("prepare");
        let ScanPlan::Local { root, options } = plan else {
            panic!("no window, so the scan runs here: {plan:?}");
        };
        let scan = run_scan(root, options).expect("scan");
        session.store_scan(scan)
    }

    fn unlinked(dir: &Path) -> Session {
        Session::detached(endpoint(dir), None)
    }

    fn worktrees(root: &Path) -> Scan {
        use disktree_core::sharing::{Allocation, CloneInfo};
        use disktree_core::tree::{Metric, NodeKind, aggregate};
        const MIB: u64 = 1024 * 1024;
        let mut tree = Draft::directory("fixture");
        for index in 0..11 {
            let name = format!("worktree-{index:02}");
            std::fs::create_dir(root.join(&name)).expect("fixture directory");
            let mut dir = Draft::directory(name);
            let bytes = 10 * 1024 * MIB - 50 * MIB;
            let mut clone = Draft::entry("shared", NodeKind::File, bytes);
            clone.allocation = Some(Box::new(Allocation {
                bytes,
                identity: None,
                links: 1,
                followed: false,
                clone: Some(CloneInfo {
                    device: 1,
                    id: 42,
                    references: 11,
                    private_bytes: Some(0),
                    data_bytes: bytes,
                }),
            }));
            dir.children.push(clone);
            dir.children.push(Draft::entry(
                "private",
                NodeKind::File,
                50 * MIB,
            ));
            tree.children.push(dir);
        }
        let mut tree = Tree::from(tree);
        aggregate(&mut tree, Metric::Bytes);
        Scan {
            root: root.into(),
            tree: Arc::new(tree),
            options: ScanOptions::default(),
            at: now(),
            elapsed: Duration::ZERO,
            unreadable: 0,
            space: None,
            device: None,
            insights: vec![],
        }
    }

    #[test]
    #[cfg(windows)]
    fn offline_reports_keep_distinct_windows_drives_separate() {
        let mut first = Plan::default();
        first.root = r"C:\fixture".into();
        let mut second = Plan::default();
        second.root = r"D:\fixture".into();
        let reports = union_reports(vec![first, second]);
        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].root, Path::new(r"C:\fixture"));
        assert_eq!(reports[1].root, Path::new(r"D:\fixture"));
    }

    #[test]
    fn standalone_union_completes_groups_across_separate_scan_roots() {
        let temp = tempfile::tempdir_in(std::env::current_dir().expect("cwd"))
            .expect("fixture");
        let root = temp.path().canonicalize().expect("root");
        let scan = worktrees(&root);
        let mut session = unlinked(&root);
        let mut paths = Vec::new();
        for node in scan.tree.root().children() {
            let root = root.join(node.name());
            std::fs::write(root.join("shared"), b"synthetic allocation")
                .expect("fixture volume identity");
            paths.push(root.join("shared"));
            session.store_scan(Scan {
                root,
                tree: Arc::new(Tree::copy_of(node)),
                options: ScanOptions::default(),
                at: now(),
                elapsed: Duration::ZERO,
                unreadable: 0,
                space: None,
                device: None,
                insights: vec![],
            });
        }
        let report = session
            .mark(&PathsParams { paths })
            .expect("mark across scans");
        let bytes = 10 * 1024 * 1024 * 1024 - 50 * 1024 * 1024;
        let range = format!("{}–{}", human_bytes(bytes), human_bytes(bytes));
        assert!(report.contains(&range), "{report}");
        assert_eq!(report.matches("Selected-set reclaim estimate:").count(), 1);
        assert!(
            report.contains(&format!("Projects {}", human_bytes(bytes))),
            "{report}"
        );
    }

    #[test]
    fn standalone_plan_projects_the_selected_union_and_reports_unknown_marks() {
        let temp = tempfile::tempdir_in(std::env::current_dir().expect("cwd"))
            .expect("fixture");
        let root = temp.path().canonicalize().expect("root");
        let scan = worktrees(&root);
        let paths: Vec<_> = scan
            .tree
            .root()
            .children()
            .iter()
            .take(10)
            .map(|node| root.join(node.name()))
            .collect();
        let mut session = unlinked(&root);
        session.store_scan(scan);
        let report = session.mark(&PathsParams { paths }).expect("mark");
        assert!(report.contains("100 GiB"), "{report}");
        assert!(report.contains("500 MiB–500 MiB"), "{report}");
        assert!(report.contains("Projects 500 MiB"), "{report}");
        assert!(!report.contains("Projects 100 GiB"), "{report}");
        assert!(report.contains("snapshots"), "{report}");
        let mut unknown = plan(
            &[Target {
                path: root.join("missing"),
                bytes: 123,
                is_dir: false,
                hidden: false,
            }],
            &root,
        );
        let estimate = session.selection_estimate(&mut unknown);
        assert_eq!(
            (estimate.lower, estimate.upper, estimate.on_volume),
            (0, Some(123), 0)
        );
        assert!(estimate.partial);
        session.scans[0].options.apparent_size = true;
        let mut unmeasured = plan(session.marks(), &root);
        let estimate = session.selection_estimate(&mut unmeasured);
        assert_eq!(estimate.lower, 0);
        assert_eq!(estimate.on_volume, 0);
        assert_eq!(estimate.upper, None);
        assert!(estimate.partial);
    }

    #[test]
    fn a_scan_reports_totals_the_top_level_and_findings() {
        let temp = fixture();
        let mut session = unlinked(temp.path());
        let report = scanned(&mut session, temp.path());
        assert!(report.starts_with("Scanned "), "{report}");
        assert!(report.contains("6 files"), "{report}");
        assert!(report.contains(".cache/"), "{report}");
        assert!(report.contains("Top level, largest first:"), "{report}");
        assert!(report.contains("Next: pwrdisk_ls"), "{report}");
        assert_eq!(session.scans().len(), 1);
        // Scanning again replaces, not duplicates.
        scanned(&mut session, temp.path());
        assert_eq!(session.scans().len(), 1);
    }

    #[test]
    fn ls_needs_a_scan_and_then_lists_largest_first() {
        let temp = fixture();
        let mut session = unlinked(temp.path());
        let missing = session
            .ls(&LsParams {
                path: temp.path().join("src"),
                ..LsParams::default()
            })
            .expect_err("nothing scanned");
        assert!(missing.contains("pwrdisk_scan"), "{missing}");

        scanned(&mut session, temp.path());
        let listing = session
            .ls(&LsParams {
                path: temp.path().to_path_buf(),
                limit: Some(2),
                show: false,
            })
            .expect("ls");
        let lines: Vec<&str> = listing.lines().collect();
        // Header, then .cache (400k), src (302.1k), then the rest.
        assert!(lines[1].contains(".cache/"), "{listing}");
        assert!(lines[2].contains("src/"), "{listing}");
        assert!(lines[3].contains("2 more"), "{listing}");

        let outside = session
            .ls(&LsParams {
                path: PathBuf::from("/definitely/elsewhere"),
                ..LsParams::default()
            })
            .expect_err("outside");
        assert!(outside.contains("outside the scanned roots"), "{outside}");
    }

    #[test]
    fn find_filters_by_kind_size_name_and_reclaim() {
        let temp = fixture();
        let mut session = unlinked(temp.path());
        scanned(&mut session, temp.path());

        let files = session
            .find(&FindParams {
                kind: Some(KindName::File),
                min_bytes: Some(200_000),
                ..FindParams::default()
            })
            .expect("find");
        assert!(files.contains("blob.bin"), "{files}");
        assert!(files.contains("image.iso"), "{files}");
        let app_path = Path::new("debug").join("app");
        assert!(files.contains(&app_path.display().to_string()), "{files}");
        assert!(!files.contains("todo.txt"), "{files}");

        let reclaim = session
            .find(&FindParams {
                reclaimable: true,
                ..FindParams::default()
            })
            .expect("find");
        // The topmost reclaimable directory, once, without its contents.
        assert!(reclaim.contains(".cache/"), "{reclaim}");
        assert!(reclaim.contains("target/"), "{reclaim}");
        assert!(!reclaim.contains("blob.bin"), "{reclaim}");

        let named = session
            .find(&FindParams {
                name: Some("ISO".into()),
                ..FindParams::default()
            })
            .expect("find");
        assert!(named.starts_with("1 match "), "{named}");

        let none = session
            .find(&FindParams {
                name: Some("nothing-here".into()),
                ..FindParams::default()
            })
            .expect("find");
        assert!(none.contains("Nothing"), "{none}");
    }

    /// A scan inside another one is not searched twice.
    #[test]
    fn find_counts_nested_scans_once() {
        let temp = fixture();
        let mut session = unlinked(temp.path());
        scanned(&mut session, temp.path());
        scanned(&mut session, &temp.path().join("src"));
        let found = session
            .find(&FindParams {
                name: Some("main.rs".into()),
                ..FindParams::default()
            })
            .expect("find");
        assert!(found.starts_with("1 match "), "{found}");
    }

    #[test]
    fn marks_are_planned_with_the_guards_and_without_a_window() {
        let temp = fixture();
        let mut session = unlinked(temp.path());
        scanned(&mut session, temp.path());

        let root_refused = session
            .mark(&PathsParams {
                paths: vec![temp.path().to_path_buf()],
            })
            .expect_err("the root alone");
        assert!(root_refused.contains("scanned root"), "{root_refused}");
        // With a good path beside it, the root is reported and the rest
        // is marked.
        let partial = session
            .mark(&PathsParams {
                paths: vec![
                    temp.path().to_path_buf(),
                    temp.path().join("notes"),
                ],
            })
            .expect("one of two");
        assert!(partial.starts_with("Not marked:"), "{partial}");
        assert!(partial.contains("Marked 1"), "{partial}");
        session
            .unmark(&PathsParams { paths: vec![] })
            .expect("clear");

        let plan = session
            .mark(&PathsParams {
                paths: vec![
                    temp.path().join(".cache"),
                    temp.path().join("src/app/target"),
                    // Inside the first mark: covered, not counted twice.
                    temp.path().join(".cache/pip"),
                ],
            })
            .expect("mark");
        assert!(plan.starts_with("Marked 3"), "{plan}");
        assert!(plan.contains("Covered by another mark (1)"), "{plan}");
        assert!(plan.contains("Selected-set reclaim estimate:"), "{plan}");
        assert!(
            plan.contains(if disktree_link::SUPPORTED {
                "Window: none open, and none can be opened"
            } else {
                "Window: no link on this system yet"
            }),
            "{plan}"
        );
        assert_eq!(session.marks().len(), 3);

        let fewer = session
            .unmark(&PathsParams {
                paths: vec![temp.path().join(".cache/pip")],
            })
            .expect("unmark");
        assert!(fewer.starts_with("Marked 2"), "{fewer}");
        let none = session
            .unmark(&PathsParams { paths: vec![] })
            .expect("unmark all");
        assert!(none.starts_with("Nothing is marked."), "{none}");

        let review = session.review().expect_err("nothing to review");
        assert!(review.contains("Nothing is marked"), "{review}");
    }

    fn estimate_json(text: &str) -> disktree_core::planning::Report {
        let (_, json) = text.split_once('\n').expect("JSON after introduction");
        serde_json::Deserializer::from_str(json)
            .into_iter()
            .next()
            .expect("report")
            .expect("valid report")
    }

    #[test]
    fn hypothetical_selection_preserves_the_offline_cart_and_scan() {
        let temp = fixture();
        let mut session = unlinked(temp.path());
        scanned(&mut session, temp.path());
        let kept = Target {
            path: temp.path().join("notes"),
            bytes: 123,
            is_dir: true,
            hidden: false,
        };
        session.marks.push(kept.clone());
        let root = session.scans[0].root.clone();
        let at = session.scans[0].at;
        let text = session
            .estimate_paths(&EstimateParams {
                paths: vec![
                    temp.path().join("Downloads"),
                    temp.path().join("Downloads"),
                ],
                ..EstimateParams::default()
            })
            .expect("estimate");
        let report = estimate_json(&text);
        assert_eq!(report.accepted, [temp.path().join("Downloads")]);
        assert_eq!(report.covered.len(), 1);
        assert!(report.reclaim.is_none(), "apparent fixture");
        assert_eq!(session.marks(), [kept]);
        assert_eq!(session.scans.len(), 1);
        assert_eq!(session.scans[0].root, root);
        assert_eq!(session.scans[0].at, at);
        assert!(session.link.is_none());
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn hypothetical_selection_reads_the_window_without_greeting_or_handover() {
        let temp = fixture();
        let socket = endpoint(temp.path());
        let window_root = temp.path().join("src");
        let mut session = Session::detached(socket.clone(), None);
        // A stale, broader local tree and offline cart must not be adopted.
        scanned(&mut session, temp.path());
        let window = FakeWindow::open(&socket, &window_root);
        let kept = Target {
            path: temp.path().join("Downloads"),
            bytes: 1,
            is_dir: true,
            hidden: false,
        };
        session.marks.push(kept.clone());
        let text = session
            .estimate_paths(&EstimateParams {
                paths: vec![
                    window_root.join("app"),
                    temp.path().join("Downloads"),
                ],
                ..EstimateParams::default()
            })
            .expect("window estimate");
        let report = estimate_json(&text);
        assert_eq!(report.context.root, window_root);
        assert_eq!(report.accepted.len(), 1);
        assert_eq!(report.refused.len(), 1);
        assert_eq!(session.marks(), [kept]);
        assert!(window.marks().is_empty());
        assert!(session.link.is_none());
        assert!(session.window.is_none());
        assert!(matches!(
            window.asked().as_slice(),
            [Request::State, Request::Estimate { .. }]
        ));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn hypothetical_selection_refuses_remote_or_old_peers_without_local_fallback()
     {
        for remote in [true, false] {
            let temp = fixture();
            let scan =
                run_scan(temp.path().to_path_buf(), apparent()).expect("scan");
            let socket = endpoint(temp.path());
            let window = FakeWindow::with_tree(
                &socket,
                FakeTree {
                    protocol: if remote { VERSION } else { VERSION - 1 },
                    root: scan.root.clone(),
                    tree: Arc::clone(&scan.tree),
                    marks: vec![],
                    remote: remote.then(|| "contrived-host".into()),
                    busy: false,
                    options: scan.options.clone(),
                    unreadable: 0,
                    removed: Vec::new(),
                },
            );
            let mut session = Session::detached(socket, None);
            session.store_scan(scan);
            let error = session
                .estimate_paths(&EstimateParams {
                    paths: vec![temp.path().join("Downloads")],
                    ..EstimateParams::default()
                })
                .expect_err("cannot substitute local tree");
            assert!(
                error.contains(if remote {
                    "SSH host"
                } else {
                    "restart/reconnect"
                }),
                "{error}"
            );
            assert!(matches!(window.asked().as_slice(), [Request::State]));
        }
    }

    /// A stand-in for the window: holds a real tree of the fixture, answers
    /// over the real socket the way the app does, and records what it was
    /// asked.
    #[cfg(any(unix, windows))]
    struct FakeWindow {
        listener: Arc<Listener>,
        asked: Arc<Mutex<Vec<Request>>>,
        marks: Arc<Mutex<Vec<Mark>>>,
        stop: Arc<AtomicBool>,
    }

    #[cfg(any(unix, windows))]
    struct FakeTree {
        protocol: u32,
        root: PathBuf,
        tree: Arc<Tree>,
        marks: Vec<Mark>,
        /// The SSH host the window shows, as a window on one answers.
        remote: Option<String>,
        /// A removal is running: marks are refused, as the app does.
        busy: bool,
        options: ScanOptions,
        unreadable: u64,
        /// What a removal since the scan took, with its scanned bytes: the
        /// window keeps it in the tree and flags it, as the app does.
        removed: Vec<(PathBuf, u64)>,
    }

    #[cfg(any(unix, windows))]
    impl FakeTree {
        fn note_removal(&self, entry: &mut disktree_link::Entry, path: &Path) {
            entry.removed =
                self.removed.iter().any(|(gone, _)| path.starts_with(gone));
            if !entry.removed {
                entry.removed_bytes = self
                    .removed
                    .iter()
                    .filter(|(gone, _)| gone.starts_with(path))
                    .map(|(_, bytes)| bytes)
                    .sum();
            }
        }

        fn state(&self, screen: &str) -> State {
            let targets: Vec<_> = self
                .marks
                .iter()
                .map(|mark| Target {
                    path: mark.path.clone(),
                    bytes: mark.bytes,
                    is_dir: mark.is_dir,
                    hidden: false,
                })
                .collect();
            let mut plan = plan(&targets, &self.root);
            plan.estimate_selection(|targets| {
                sharing::estimate(targets.iter().filter_map(|target| {
                    target
                        .path
                        .strip_prefix(&self.root)
                        .ok()
                        .and_then(|relative| self.tree.root().descend(relative))
                        .map(|(_, node)| node)
                }))
            });
            State {
                version: self.protocol,
                app_version: "0.13.0".into(),
                root: self.root.clone(),
                current: self.root.clone(),
                screen: screen.into(),
                scanning: false,
                scanned: true,
                estimate: Some(SelectionEstimate::of(&plan)),
                marks: self.marks.clone(),
                space: None,
                removal: Some(Removal {
                    mode: "trash".into(),
                    removed: 2,
                    failed: 0,
                    bytes: 700_000,
                    freed: Some(690_000),
                    at: now() - 120,
                }),
                removals: Some(disktree_link::RemovalLedger {
                    cohorts: vec![disktree_link::RemovalCohort {
                        mode: "trash".into(),
                        removed: 2,
                        failed: 0,
                        reported: 700_000,
                        private: Some(650_000),
                        observed: Some(690_000),
                        started: now() - 125,
                        finished: now() - 120,
                    }],
                    reported: 700_000,
                    private: Some(650_000),
                    observed: Some(690_000),
                }),
                tree: Some(TreeSummary {
                    bytes: self.tree.root().bytes,
                    files: self.tree.root().files,
                    dirs: u64::from(self.tree.root().dirs),
                    scanned_at: now() - 30,
                    elapsed_ms: 1_500,
                    unreadable: self.unreadable,
                    apparent_size: self.options.apparent_size,
                    include_hidden: self.options.include_hidden,
                }),
                insights: vec![Insight {
                    path: self.root.join(".cache"),
                    bytes: 400_000,
                    what: "reclaimable: cache".into(),
                }],
                remote: self.remote.clone(),
            }
        }

        fn answer(&mut self, request: &Request) -> Response {
            if let Some(host) = &self.remote
                && !matches!(request, Request::Hello { .. } | Request::State)
            {
                return Response::error(disktree_link::remote_refusal(host));
            }
            if self.busy && matches!(request, Request::Mark { .. }) {
                return Response::error("a removal is running");
            }
            match request {
                Request::Hello { .. } | Request::State => {
                    Response::State(self.state("explore"))
                }
                Request::Scan { root, .. } => {
                    self.tree = Arc::new(
                        disktree_core::scan::scan(root, apparent())
                            .expect("scan"),
                    );
                    self.root.clone_from(root);
                    Response::State(self.state("explore"))
                }
                Request::Ls { path, limit } => {
                    let Some((_, node)) =
                        path.strip_prefix(&self.root).ok().and_then(
                            |relative| self.tree.root().descend(relative),
                        )
                    else {
                        return Response::error("not in the window's tree");
                    };
                    let mut listing = Listing::of(
                        &self.root,
                        self.tree.root().bytes,
                        path,
                        node,
                        *limit,
                    );
                    self.note_removal(&mut listing.entry, path);
                    for child in &mut listing.children {
                        self.note_removal(child, &path.join(&child.name));
                    }
                    Response::Listing(listing)
                }
                Request::Find(request) => {
                    let filter = match request.filter() {
                        Ok(filter) => filter,
                        Err(message) => return Response::error(message),
                    };
                    let found = query::find(
                        &self.root,
                        self.tree.root(),
                        &filter,
                        request.limit,
                    );
                    let mut found = FoundList::of(&found);
                    for item in &mut found.matches {
                        self.note_removal(&mut item.entry, &item.path);
                    }
                    Response::Found(found)
                }
                Request::Estimate {
                    paths,
                    peer_scope,
                    peer_limit,
                } => {
                    let context = disktree_core::planning::Context::of(
                        &self.root,
                        &self.options,
                        1,
                        self.unreadable,
                    );
                    match disktree_core::planning::estimate(
                        &self.tree,
                        context,
                        paths,
                        peer_scope.as_deref(),
                        *peer_limit,
                    ) {
                        Ok(report) => Response::Estimate(report),
                        Err(message) => Response::error(message),
                    }
                }
                Request::Mark { targets, .. } => {
                    let mut refused = Vec::new();
                    for target in targets {
                        let node =
                            target.path.strip_prefix(&self.root).ok().and_then(
                                |relative| self.tree.root().descend(relative),
                            );
                        let Some((_, node)) = node else {
                            refused.push(Refused {
                                path: target.path.clone(),
                                reason: "not in the window's tree".into(),
                            });
                            continue;
                        };
                        if !self.marks.iter().any(|m| m.path == target.path) {
                            self.marks.push(Mark {
                                path: target.path.clone(),
                                bytes: node.bytes,
                                is_dir: node.is_dir(),
                            });
                        }
                    }
                    Response::Marked {
                        state: self.state("explore"),
                        refused,
                    }
                }
                Request::Unmark { paths } if paths.is_empty() => {
                    self.marks.clear();
                    Response::State(self.state("explore"))
                }
                Request::Unmark { paths } => {
                    self.marks.retain(|mark| !paths.contains(&mark.path));
                    Response::State(self.state("explore"))
                }
                Request::Show { .. } | Request::Review => Response::Ok,
                Request::PrepareRemoval { .. } | Request::Remove(_) => {
                    Response::error(
                        "removal is unavailable in the stand-in window",
                    )
                }
            }
        }
    }

    #[cfg(any(unix, windows))]
    impl FakeWindow {
        fn open(socket: &Path, root: &Path) -> Self {
            Self::showing(socket, root, None)
        }

        /// A window showing `remote`, an SSH host, rather than this
        /// computer.
        fn showing(socket: &Path, root: &Path, remote: Option<&str>) -> Self {
            Self::serving(socket, root, remote, false)
        }

        /// A window in the middle of a removal, refusing new marks.
        fn removing(socket: &Path, root: &Path) -> Self {
            Self::serving(socket, root, None, true)
        }

        fn serving(
            socket: &Path,
            root: &Path,
            remote: Option<&str>,
            busy: bool,
        ) -> Self {
            let fake = FakeTree {
                protocol: VERSION,
                root: root.to_path_buf(),
                tree: Arc::new(
                    disktree_core::scan::scan(root, apparent()).expect("scan"),
                ),
                marks: Vec::new(),
                remote: remote.map(str::to_owned),
                busy,
                options: apparent(),
                unreadable: 0,
                removed: Vec::new(),
            };
            Self::with_tree(socket, fake)
        }

        fn with_tree(socket: &Path, mut fake: FakeTree) -> Self {
            let listener = Arc::new(Listener::bind_at(socket).expect("bind"));
            let asked = Arc::new(Mutex::new(Vec::new()));
            let marks: Arc<Mutex<Vec<Mark>>> = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (l, a, m, s) = (
                Arc::clone(&listener),
                Arc::clone(&asked),
                Arc::clone(&marks),
                Arc::clone(&stop),
            );
            thread::spawn(move || {
                while !s.load(Ordering::Relaxed) {
                    for incoming in l.drain() {
                        if let Incoming::Request { request, reply, .. } =
                            incoming
                        {
                            a.lock().expect("asked").push(request.clone());
                            let response = fake.answer(&request);
                            m.lock().expect("marks").clone_from(&fake.marks);
                            reply.send(response);
                        }
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            });
            Self {
                listener,
                asked,
                marks,
                stop,
            }
        }

        fn asked(&self) -> Vec<Request> {
            self.asked.lock().expect("asked").clone()
        }

        fn marks(&self) -> Vec<Mark> {
            self.marks.lock().expect("marks").clone()
        }
    }

    #[cfg(any(unix, windows))]
    impl Drop for FakeWindow {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = &self.listener;
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn linked_plan_uses_window_union_even_with_a_conflicting_local_scan() {
        let temp = tempfile::tempdir_in(std::env::current_dir().expect("cwd"))
            .expect("fixture");
        let root = temp.path().canonicalize().expect("root");
        let scan = worktrees(&root);
        let socket_dir = tempfile::tempdir().expect("socket fixture");
        let socket = endpoint(socket_dir.path());
        let _window = FakeWindow::with_tree(
            &socket,
            FakeTree {
                protocol: VERSION,
                root: root.clone(),
                tree: Arc::clone(&scan.tree),
                marks: vec![],
                remote: None,
                busy: false,
                options: ScanOptions::default(),
                unreadable: 0,
                removed: Vec::new(),
            },
        );
        let paths: Vec<_> = scan
            .tree
            .root()
            .children()
            .iter()
            .take(10)
            .map(|node| root.join(node.name()))
            .collect();
        let mut session = Session::detached(socket, None);
        // The agent's old scan has no sharing facts; the window owns truth.
        let mut stale = scan;
        let mut draft = stale.tree.root().to_draft();
        for dir in &mut draft.children {
            dir.children[0].allocation = None;
        }
        let mut tree = Tree::from(draft);
        disktree_core::tree::aggregate(
            &mut tree,
            disktree_core::tree::Metric::Bytes,
        );
        stale.tree = Arc::new(tree);
        session.store_scan(stale);
        let report = session.mark(&PathsParams { paths }).expect("linked mark");
        assert!(report.contains("500 MiB–500 MiB"), "{report}");
        assert!(report.contains("Projects 500 MiB"), "{report}");
        let status = session.status();
        assert!(status.contains("0.13.0"), "{status}");
        assert!(status.contains("Release mismatch:"), "{status}");
        assert!(status.contains("connection remains usable"), "{status}");
        assert!(status.contains("restart/reconnect"), "{status}");
        assert!(
            status.contains(concat!(env!("CARGO_PKG_VERSION"), " MCP server")),
            "{status}"
        );
        let review = session.review().expect("review");
        assert!(review.contains("500 MiB–500 MiB"), "{review}");
        assert!(!review.contains("100 GiB to free"), "{review}");

        // Incomplete, missing or mismatched link state never falls back to
        // the raw allocation or the agent's conflicting local scan.
        for failure in ["scanning", "missing", "changed"] {
            let window = session.window.as_mut().expect("window");
            match failure {
                "scanning" => window.scanning = true,
                "missing" => {
                    window.scanning = false;
                    window.estimate = None;
                }
                _ => {
                    window.estimate = Some(SelectionEstimate {
                        paths: vec![],
                        lower: u64::MAX,
                        upper: Some(u64::MAX),
                        on_volume: u64::MAX,
                        partial: false,
                        shared: 0,
                    });
                }
            }
            let mut plan = plan(session.marks(), &root);
            let estimate = session.selection_estimate(&mut plan);
            assert_eq!(estimate.on_volume, 0, "{failure}");
            assert_eq!(estimate.lower, 0, "{failure}");
            assert!(estimate.partial, "{failure}");
        }
        let mut wire =
            serde_json::to_value(session.window.as_ref().expect("window"))
                .expect("state JSON");
        let fields = wire.as_object_mut().expect("object");
        fields.remove("app_version");
        fields.remove("estimate");
        let mut state: State =
            serde_json::from_value(wire).expect("optional metadata");
        assert!(state.app_version.is_empty());
        assert!(version_notice(&state).contains("version unavailable"));
        state.app_version = env!("CARGO_PKG_VERSION").into();
        assert!(version_notice(&state).is_empty());
        session.window = Some(state);
        let mut plan = plan(session.marks(), &root);
        let estimate = session.selection_estimate(&mut plan);
        assert_eq!(
            (estimate.lower, estimate.upper, estimate.on_volume),
            (0, None, 0)
        );
        assert!(estimate.partial);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn incompatible_state_is_refused_before_adopting_marks_or_handing_over() {
        let temp = fixture();
        let socket = endpoint(temp.path());
        let window = FakeWindow::with_tree(
            &socket,
            FakeTree {
                protocol: VERSION - 1,
                root: temp.path().into(),
                tree: Arc::new(
                    disktree_core::scan::scan(temp.path(), apparent())
                        .expect("scan"),
                ),
                marks: vec![],
                remote: None,
                busy: false,
                options: apparent(),
                unreadable: 0,
                removed: Vec::new(),
            },
        );
        let mut session = Session::detached(socket, None);
        let mark = Target {
            path: temp.path().join("notes"),
            bytes: 500,
            is_dir: true,
            hidden: false,
        };
        session.marks.push(mark.clone());
        assert!(!session.try_connect());
        assert!(session.link.is_none() && session.window.is_none());
        assert_eq!(session.marks(), &[mark]);
        let reason = session.unlinked.as_deref().expect("explicit refusal");
        assert!(
            reason.contains("Incompatible PwrDisk link protocols"),
            "{reason}"
        );
        assert!(reason.contains("restart/reconnect"), "{reason}");
        assert!(reason.contains("updated installation"), "{reason}");
        assert!(
            window
                .asked()
                .iter()
                .all(|r| matches!(r, Request::Hello { .. }))
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn status_reuses_window_measurement_context_without_scanning() {
        let temp = fixture();
        let tree = Arc::new(
            disktree_core::scan::scan(temp.path(), apparent())
                .expect("fixture scan"),
        );
        for (apparent_size, include_hidden, unreadable) in
            [(true, false, 7), (false, true, 0)]
        {
            let socket_dir = tempfile::TempDir::new().expect("socket dir");
            let socket = endpoint(socket_dir.path());
            let window = FakeWindow::with_tree(
                &socket,
                FakeTree {
                    protocol: VERSION,
                    root: temp.path().to_path_buf(),
                    tree: Arc::clone(&tree),
                    marks: Vec::new(),
                    remote: None,
                    busy: false,
                    options: ScanOptions {
                        apparent_size,
                        include_hidden,
                        ..ScanOptions::default()
                    },
                    unreadable,
                    removed: Vec::new(),
                },
            );
            let mut session = Session::detached(socket, None);
            let status = session.status();
            assert!(
                status.contains(if apparent_size {
                    "apparent sizes"
                } else {
                    "allocated sizes"
                }),
                "{status}"
            );
            assert!(
                status.contains(if include_hidden {
                    "hidden entries included"
                } else {
                    "hidden entries skipped"
                }),
                "{status}"
            );
            assert!(
                status.contains(&format!("{unreadable} unreadable")),
                "{status}"
            );
            if cfg!(target_os = "macos") {
                assert!(
                    status.contains("APFS clones share blocks"),
                    "{status}"
                );
                assert!(
                    status.contains("not guaranteed freed space"),
                    "{status}"
                );
            }
            assert!(session.scans().is_empty());
            assert!(session.marks().is_empty());
            assert!(window.asked().iter().all(|request| matches!(
                request,
                Request::Hello { .. } | Request::State
            )));
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn private_tmp_is_a_candidate_but_guards_refuse_it_locally_and_linked() {
        use disktree_core::{
            classify::classify,
            tree::{Metric, NodeKind, aggregate},
        };
        // Only a synthetic tree represents /; no scan, write or mark of real data.
        let mut root = Draft::directory("/");
        let mut private = Draft::directory("private");
        let mut tmp = Draft::directory("tmp");
        tmp.children
            .push(Draft::entry("fixture", NodeKind::File, 123));
        private.children.push(tmp);
        root.children.push(private);
        let mut root = Tree::from(root);
        aggregate(&mut root, Metric::Bytes);
        classify(&mut root);
        let tree = Arc::new(root);
        for linked in [false, true] {
            let temp = tempfile::TempDir::new().expect("endpoint");
            let socket = endpoint(temp.path());
            let window = linked.then(|| {
                FakeWindow::with_tree(
                    &socket,
                    FakeTree {
                        protocol: VERSION,
                        root: PathBuf::from("/"),
                        tree: Arc::clone(&tree),
                        marks: Vec::new(),
                        remote: None,
                        busy: false,
                        options: apparent(),
                        unreadable: 0,
                        removed: Vec::new(),
                    },
                )
            });
            let mut session = Session::detached(socket, None);
            if !linked {
                session.scans.push(Scan {
                    root: PathBuf::from("/"),
                    tree: Arc::clone(&tree),
                    options: apparent(),
                    at: now(),
                    elapsed: Duration::ZERO,
                    unreadable: 0,
                    space: None,
                    device: None,
                    insights: Vec::new(),
                });
            }
            let report = session
                .find(&FindParams {
                    reclaimable: true,
                    ..FindParams::default()
                })
                .expect("find");
            assert!(report.starts_with("1 match "), "{report}");
            assert!(
                report.contains("cleanup candidate: temporary"),
                "{report}"
            );
            assert!(report.contains("Removal blocked: /private/tmp — part of the system under /private"), "{report}");
            assert!(report.contains("not guaranteed freed space"), "{report}");
            assert!(session.marks().is_empty());
            if let Some(window) = window {
                assert!(window.marks().is_empty());
                assert!(window.asked().iter().all(|request| matches!(
                    request,
                    Request::Hello { .. } | Request::Find(_)
                )));
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_allocation_is_reported_even_above_volume_usage() {
        let temp = fixture();
        let mut session = unlinked(temp.path());
        scanned(&mut session, temp.path());
        let mut facts = ScanFacts::of_local(&session.scans()[0]);
        facts.tree.bytes = 2_000_000;
        facts.tree.apparent_size = false;
        facts.space = Some(Space {
            total: 1_000_000,
            free: 500_000,
            available: 500_000,
        });
        let report = session.scan_report(&facts);
        assert!(report.contains("1.9 MiB"), "{report}");
        assert!(report.contains("400% of what is used"), "{report}");
        assert!(
            report.contains("summed allocation can exceed volume usage"),
            "{report}"
        );
        assert!(report.contains("not guaranteed freed space"), "{report}");
    }

    /// Three directories, each holding one full clone of a group APFS
    /// counts `references` members of, beside a plain directory.
    fn clone_copies(root: &Path, references: u32) -> Scan {
        use disktree_core::sharing::{Allocation, CloneInfo};
        use disktree_core::tree::{Metric, NodeKind, aggregate};
        const MIB: u64 = 1024 * 1024;
        let mut tree = Draft::directory("fixture");
        for name in ["copy-a", "copy-b", "copy-c", "plain"] {
            std::fs::create_dir(root.join(name)).expect("fixture directory");
            let mut dir = Draft::directory(name);
            let mut file = Draft::entry("data", NodeKind::File, 12 * MIB);
            if name != "plain" {
                file.allocation = Some(Box::new(Allocation {
                    bytes: 12 * MIB,
                    identity: None,
                    links: 1,
                    followed: false,
                    clone: Some(CloneInfo {
                        device: 1,
                        id: 9,
                        references,
                        private_bytes: Some(0),
                        data_bytes: 12 * MIB,
                    }),
                }));
            }
            dir.children.push(file);
            tree.children.push(dir);
        }
        let mut tree = Tree::from(tree);
        aggregate(&mut tree, Metric::Bytes);
        Scan {
            root: root.into(),
            tree: Arc::new(tree),
            options: ScanOptions::default(),
            at: now(),
            elapsed: Duration::ZERO,
            unreadable: 0,
            space: None,
            device: None,
            insights: vec![],
        }
    }

    fn copies(root: &Path) -> Vec<PathBuf> {
        ["copy-a", "copy-b", "copy-c"]
            .iter()
            .map(|name| root.join(name))
            .collect()
    }

    #[test]
    fn clone_rows_split_shared_bytes_and_a_complete_group_frees_one_copy() {
        let temp = tempfile::tempdir_in(std::env::current_dir().expect("cwd"))
            .expect("fixture");
        let root = temp.path().canonicalize().expect("root");
        let mut session = unlinked(&root);
        let report = session.store_scan(clone_copies(&root, 3));
        assert!(
            report.contains("APFS clones: 36 MiB of this is shared"),
            "{report}"
        );
        assert!(
            report.contains("\u{2248} 24 MiB is counted again"),
            "{report}"
        );
        let listing = session
            .ls(&LsParams {
                path: root.clone(),
                limit: None,
                show: false,
            })
            .expect("ls");
        let row = |name: &str| {
            listing
                .lines()
                .find(|line| line.contains(&format!(" {name}/ ")))
                .unwrap_or_else(|| panic!("{name} in {listing}"))
                .to_string()
        };
        for name in ["copy-a", "copy-b", "copy-c"] {
            assert!(
                row(name).contains(
                    "0 B private \u{00b7} 12 MiB shared with other APFS \
                     clones \u{00b7} frees \u{2248} 0 B alone"
                ),
                "{listing}"
            );
        }
        assert!(
            row("plain").ends_with("  plain/  1 files \u{00b7} age unknown")
        );
        let found = session
            .find(&FindParams {
                path: Some(root.clone()),
                kind: Some(KindName::Dir),
                ..FindParams::default()
            })
            .expect("find");
        assert_eq!(found.matches("shared with other APFS clones").count(), 3);

        let one = session
            .mark(&PathsParams {
                paths: vec![root.join("copy-a")],
            })
            .expect("mark one");
        assert!(one.contains("estimate: 0 B\u{2013}0 B"), "{one}");
        assert!(one.contains("12 MiB of the selection is APFS clone"));
        assert!(!one.contains("Frees"), "{one}");
        session.unmark(&PathsParams::default()).expect("unmark");
        let all = session
            .mark(&PathsParams {
                paths: copies(&root),
            })
            .expect("mark all");
        assert!(all.contains("estimate: 12 MiB\u{2013}12 MiB"), "{all}");
        assert!(all.contains("36 MiB of the selection is APFS clone"));
    }

    /// Three 10 GiB worktrees sharing all but 100 MiB each. `modified`
    /// puts the private 100 MiB inside the clone file (a rewritten
    /// clone); otherwise it is a separate, ordinary file beside a full
    /// clone.
    fn worktree_trio(root: &Path, modified: bool) -> Scan {
        use disktree_core::sharing::{Allocation, CloneInfo};
        use disktree_core::tree::{Metric, NodeKind, aggregate};
        const MIB: u64 = 1024 * 1024;
        let mut tree = Draft::directory("fixture");
        for name in ["wt-a", "wt-b", "wt-c"] {
            std::fs::create_dir(root.join(name)).expect("fixture directory");
            let mut dir = Draft::directory(name);
            let (bytes, private) = if modified {
                (10_240 * MIB, 100 * MIB)
            } else {
                (10_140 * MIB, 0)
            };
            let mut file = Draft::entry("objects", NodeKind::File, bytes);
            file.allocation = Some(Box::new(Allocation {
                bytes,
                identity: None,
                links: 1,
                followed: false,
                clone: Some(CloneInfo {
                    device: 1,
                    id: 77,
                    references: 3,
                    private_bytes: Some(private),
                    data_bytes: bytes,
                }),
            }));
            dir.children.push(file);
            if !modified {
                dir.children.push(Draft::entry(
                    "own",
                    NodeKind::File,
                    100 * MIB,
                ));
            }
            tree.children.push(dir);
        }
        let mut tree = Tree::from(tree);
        aggregate(&mut tree, Metric::Bytes);
        Scan {
            root: root.into(),
            tree: Arc::new(tree),
            options: ScanOptions::default(),
            at: now(),
            elapsed: Duration::ZERO,
            unreadable: 0,
            space: None,
            device: None,
            insights: vec![],
        }
    }

    #[test]
    fn marking_a_worktree_trio_one_by_one_frees_one_copy_at_the_end() {
        const MIB: u64 = 1024 * 1024;
        for modified in [false, true] {
            let temp =
                tempfile::tempdir_in(std::env::current_dir().expect("cwd"))
                    .expect("fixture");
            let root = temp.path().canonicalize().expect("root");
            let mut session = unlinked(&root);
            session.store_scan(worktree_trio(&root, modified));
            let mut seen = Vec::new();
            for name in ["wt-a", "wt-b", "wt-c"] {
                let report = session
                    .mark(&PathsParams {
                        paths: vec![root.join(name)],
                    })
                    .expect("mark");
                let mut plan = plan(session.marks(), &root);
                let estimate = session.selection_estimate(&mut plan);
                assert_eq!(estimate.on_volume, estimate.lower, "{report}");
                seen.push((estimate.lower, estimate.upper));
            }
            // Each worktree's own 100 MiB until the last clone of the
            // group is selected; then the shared data counts, once.
            let lower: Vec<u64> =
                seen.iter().map(|(lower, _)| *lower).collect();
            assert_eq!(
                lower,
                [100 * MIB, 200 * MIB, (300 + 10_140) * MIB],
                "modified={modified}"
            );
            if !modified {
                // Full clones beside ordinary files: exact, no range.
                let upper: Vec<_> =
                    seen.iter().map(|(_, upper)| *upper).collect();
                assert_eq!(
                    upper,
                    [Some(100 * MIB), Some(200 * MIB), Some(10_440 * MIB)]
                );
            }
            eprintln!("modified={modified}: {seen:?}");
        }
    }

    #[test]
    fn a_clone_outside_the_scan_keeps_every_shared_block() {
        let temp = tempfile::tempdir_in(std::env::current_dir().expect("cwd"))
            .expect("fixture");
        let root = temp.path().canonicalize().expect("root");
        let mut session = unlinked(&root);
        session.store_scan(clone_copies(&root, 4));
        let all = session
            .mark(&PathsParams {
                paths: copies(&root),
            })
            .expect("mark all");
        assert!(all.contains("estimate: 0 B\u{2013}0 B"), "{all}");
        assert!(all.contains("outside this scan, keeps them"), "{all}");
        session.unmark(&PathsParams::default()).expect("unmark");
        let plain = session
            .mark(&PathsParams {
                paths: vec![root.join("plain")],
            })
            .expect("mark plain");
        assert!(plain.contains("estimate: 12 MiB\u{2013}12 MiB"), "{plain}");
        assert!(!plain.contains("APFS clone"), "{plain}");
    }

    /// The reproduction behind the clone rows: `cp -c -R` twice and a plain
    /// `cp -R`, scanned for real. Skipped where the temporary directory
    /// cannot clone (not APFS).
    #[cfg(target_os = "macos")]
    #[test]
    fn real_apfs_clones_read_as_shared_and_free_nothing_alone() {
        const MIB: u64 = 1_000_000;
        // The system-tree guard refuses canonical /private, so stay in the
        // checkout, which is APFS on any Mac that builds this.
        let temp = tempfile::tempdir_in(std::env::current_dir().expect("cwd"))
            .expect("fixture");
        let root = temp.path().canonicalize().expect("root");
        let src = root.join("src");
        std::fs::create_dir(&src).expect("mkdir");
        for index in 0..12_u8 {
            // Distinct, incompressible-enough content per file.
            let data: Vec<u8> = (0..MIB)
                .map(|i| (i as u8).wrapping_mul(31).wrapping_add(index))
                .collect();
            std::fs::write(src.join(format!("f{index}.bin")), data)
                .expect("write");
        }
        let copy = |flags: &[&str], to: &str| {
            std::process::Command::new("cp")
                .args(flags)
                .arg(&src)
                .arg(root.join(to))
                .status()
                .expect("cp")
                .success()
        };
        if !copy(&["-c", "-R"], "clone-a") {
            eprintln!("skipped: {} cannot clone files", root.display());
            return;
        }
        assert!(copy(&["-c", "-R"], "clone-b"));
        assert!(copy(&["-R"], "plain"));
        let scan =
            run_scan(root.clone(), ScanOptions::default()).expect("scan");
        let listing = Listing::of(
            &scan.root,
            scan.tree.root().bytes,
            &root,
            scan.tree.root(),
            10,
        );
        let entry = |name: &str| {
            listing
                .children
                .iter()
                .find(|child| child.name == name)
                .unwrap_or_else(|| panic!("{name}"))
                .clone()
        };
        let clone = entry("clone-a").clones.expect("clone facts");
        assert!(clone.private <= 8 * 1024, "{clone:?}");
        assert!(clone.shared >= 12 * MIB - 64 * 1024, "{clone:?}");
        assert_eq!(entry("plain").clones, None);
        let mut session = unlinked(&root);
        session.store_scan(scan);
        let marked = |session: &mut Session, name: &str| {
            let report = session
                .mark(&PathsParams {
                    paths: vec![root.join(name)],
                })
                .expect("mark");
            session.unmark(&PathsParams::default()).expect("unmark");
            let mut plan = plan(
                &[Target {
                    path: root.join(name),
                    bytes: entry(name).bytes,
                    is_dir: true,
                    hidden: false,
                }],
                &root,
            );
            (report, session.selection_estimate(&mut plan))
        };
        let (report, estimate) = marked(&mut session, "clone-a");
        assert!(estimate.lower < 64 * 1024, "{report}");
        assert!(
            report.contains("of the selection is APFS clone"),
            "{report}"
        );
        let (report, estimate) = marked(&mut session, "plain");
        assert!(estimate.lower >= 12 * MIB, "{report}");
        assert!(!report.contains("APFS clone"), "{report}");

        // Without allocated blocks there is nothing to split.
        let mut apparent = unlinked(&root);
        let report = apparent.store_scan(
            run_scan(
                root.clone(),
                ScanOptions {
                    apparent_size: true,
                    ..ScanOptions::default()
                },
            )
            .expect("apparent scan"),
        );
        assert!(!report.contains("private"), "{report}");
        assert!(!report.contains("APFS clones:"), "{report}");
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn with_a_window_open_its_tree_is_what_the_agent_reads() {
        let temp = fixture();
        let socket = endpoint(temp.path());
        let window = FakeWindow::open(&socket, temp.path());
        let mut session = Session::detached(socket, None);
        session.set_client("claude-code");
        // No scan here at all: everything comes from the window.
        assert!(session.scans().is_empty());

        let listing = session
            .ls(&LsParams {
                path: temp.path().join("src"),
                limit: Some(5),
                show: false,
            })
            .expect("ls over the link");
        assert!(listing.contains("app/"), "{listing}");
        assert!(listing.contains("% of "), "{listing}");

        let outside = session
            .ls(&LsParams {
                path: PathBuf::from("/definitely/elsewhere"),
                ..LsParams::default()
            })
            .expect_err("outside the window");
        assert!(outside.contains("window's tree"), "{outside}");
        assert!(outside.contains("pwrdisk_scan"), "{outside}");

        let reclaim = session
            .find(&FindParams {
                reclaimable: true,
                ..FindParams::default()
            })
            .expect("find over the link");
        assert!(reclaim.contains(".cache/"), "{reclaim}");
        assert!(reclaim.contains("target/"), "{reclaim}");

        // The window measures the mark; a path it does not have is refused.
        let plan = session
            .mark(&PathsParams {
                paths: vec![
                    temp.path().join(".cache"),
                    temp.path().join("not-there"),
                ],
            })
            .expect("mark");
        assert!(plan.starts_with("Not marked:"), "{plan}");
        assert!(plan.contains("not in the window's tree"), "{plan}");
        assert!(plan.contains("Marked 1"), "{plan}");
        assert_eq!(window.marks()[0].bytes, 400_000);
        assert_eq!(session.marks()[0].bytes, 400_000);

        // A scan goes to the window and is read back from it.
        let plan = session
            .prepare_scan(&ScanParams {
                path: Some(temp.path().to_path_buf()),
                apparent_size: true,
                ..ScanParams::default()
            })
            .expect("prepare");
        let ScanPlan::Window { root } = plan else {
            panic!("a linked window scans: {plan:?}");
        };
        let report = session
            .window_scan(&root)
            .expect("poll")
            .expect("the fake finishes at once");
        assert!(report.starts_with("Scanned "), "{report}");
        assert!(report.contains("6 files"), "{report}");
        assert!(report.contains("Top level, largest first:"), "{report}");
        assert!(report.contains("reclaimable: cache"), "{report}");
        assert!(report.contains("Window: open on"), "{report}");
        assert!(session.scans().is_empty(), "nothing is scanned here");

        let status = session.status();
        assert!(status.contains("Claude Code"), "{status}");
        assert!(status.contains("Last removal: trash"), "{status}");
        assert!(status.contains("Removed since the scan"), "{status}");
        assert!(status.contains("Total: 684 KiB reported"), "{status}");
        assert!(status.contains("scanned "), "{status}");
        assert!(!status.contains("Scans: none yet"), "{status}");

        let asked = window.asked();
        assert!(
            matches!(asked[0], Request::Hello { ref client, .. } if client == "Claude Code")
        );
        for wanted in ["ls", "find", "mark", "scan"] {
            assert!(
                asked.iter().any(|request| match request {
                    Request::Ls { .. } => wanted == "ls",
                    Request::Find(_) => wanted == "find",
                    Request::Mark { .. } => wanted == "mark",
                    Request::Scan { .. } => wanted == "scan",
                    _ => false,
                }),
                "{wanted} went to the window: {asked:?}"
            );
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn listings_say_what_the_window_removed_since_its_scan() {
        let temp = fixture();
        let socket = endpoint(temp.path());
        let mut fake = FakeTree {
            protocol: VERSION,
            root: temp.path().to_path_buf(),
            tree: Arc::new(
                disktree_core::scan::scan(temp.path(), apparent())
                    .expect("scan"),
            ),
            marks: Vec::new(),
            remote: None,
            busy: false,
            options: apparent(),
            unreadable: 0,
            removed: Vec::new(),
        };
        fake.removed = vec![(temp.path().join(".cache/pip"), 400_000)];
        let _window = FakeWindow::with_tree(&socket, fake);
        let mut session = Session::detached(socket, None);
        session.set_client("claude-code");

        let root = session
            .ls(&LsParams {
                path: temp.path().to_path_buf(),
                limit: Some(10),
                show: false,
            })
            .expect("ls");
        let cache = root
            .lines()
            .find(|line| line.contains(".cache/"))
            .expect("still listed");
        assert!(
            cache.contains("of it removed since the scan"),
            "part of it went: {root}"
        );
        let notes = root.lines().find(|l| l.contains("notes/")).unwrap();
        assert!(!notes.contains("removed"), "neighbours unchanged: {root}");

        let inside = session
            .ls(&LsParams {
                path: temp.path().join(".cache"),
                limit: Some(10),
                show: false,
            })
            .expect("ls");
        let pip = inside.lines().find(|l| l.contains("pip/")).unwrap();
        assert!(pip.contains("removed since the scan"), "{inside}");
        assert!(!pip.contains("of it removed"), "it went whole: {inside}");

        let found = session
            .find(&FindParams {
                name: Some("blob".into()),
                ..FindParams::default()
            })
            .expect("find");
        assert!(found.contains("removed since the scan"), "{found}");
    }

    #[test]
    #[cfg(any(unix, windows))]
    fn with_a_window_open_marks_live_there_and_review_opens_it() {
        let temp = fixture();
        let socket = endpoint(temp.path());
        let window = FakeWindow::open(&socket, temp.path());
        let mut session = Session::detached(socket, None);

        let plan = session
            .mark(&PathsParams {
                paths: vec![temp.path().join(".cache")],
            })
            .expect("mark");
        assert!(plan.starts_with("Marked 1"), "{plan}");
        assert!(plan.contains("Window: open on"), "{plan}");
        assert!(plan.contains("1 mark"), "{plan}");

        let shown = session
            .ls(&LsParams {
                path: temp.path().join("src"),
                limit: None,
                show: true,
            })
            .expect("ls with show");
        assert!(shown.contains("Listings never move"), "{shown}");

        let review = session.review().expect("review");
        assert!(review.contains("review screen is open"), "{review}");

        let asked = window.asked();
        assert!(
            !asked
                .iter()
                .any(|request| matches!(request, Request::Show { .. }))
        );
        assert!(
            asked
                .iter()
                .any(|request| matches!(request, Request::Review))
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn marks_made_before_the_window_opened_are_handed_over() {
        let temp = fixture();
        let socket = endpoint(temp.path());
        let mut session = Session::detached(socket.clone(), None);
        scanned(&mut session, temp.path());
        session
            .mark(&PathsParams {
                paths: vec![temp.path().join("Downloads")],
            })
            .expect("mark offline");
        assert_eq!(session.marks().len(), 1);

        let window = FakeWindow::open(&socket, temp.path());
        let review = session.review().expect("review");
        assert!(review.contains("1 marked"), "{review}");
        let asked = window.asked();
        let handed = asked.iter().find_map(|request| match request {
            Request::Mark { targets, .. } => Some(targets.clone()),
            _ => None,
        });
        assert_eq!(
            handed.map(|targets| targets[0].path.clone()),
            Some(temp.path().join("Downloads"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn covered_marks_use_the_windows_root_for_handover() {
        let temp = fixture();
        let socket = temp.path().join("window.sock");
        let mut session = Session::detached(socket.clone(), None);
        scanned(&mut session, temp.path());
        session
            .mark(&PathsParams {
                paths: vec![temp.path().join("src/app/target")],
            })
            .expect("mark offline");

        let root = temp.path().join("src");
        let window = FakeWindow::open(&socket, &root);
        session.status();
        assert_eq!(session.marks().len(), 1);
        assert_eq!(window.marks().len(), 1);
        assert!(window.asked().iter().all(|request| match request {
            Request::Mark { root: sent, .. } => *sent == root,
            Request::Hello { .. } | Request::State => true,
            _ => false,
        }));
    }

    #[cfg(unix)]
    #[test]
    fn linking_never_moves_the_window_to_cover_earlier_marks() {
        let temp = fixture();
        let socket = temp.path().join("window.sock");
        let mut session = Session::detached(socket.clone(), None);
        // Separate scan roots used to cause a Mark request for each root,
        // leaving the user's window on the last one.
        scanned(&mut session, &temp.path().join(".cache"));
        scanned(&mut session, &temp.path().join("src"));
        let paths = vec![
            temp.path().join(".cache/pip"),
            temp.path().join("src/app/target"),
        ];
        session
            .mark(&PathsParams {
                paths: paths.clone(),
            })
            .expect("mark offline");

        // One mark is covered, but none should be sent until all fit.
        let window = FakeWindow::open(&socket, &temp.path().join(".cache"));
        let status = session.status();
        assert!(status.contains("not followed"), "{status}");
        assert!(status.contains("pwrdisk_scan"), "{status}");
        let report = session.plan_report();
        for path in &paths {
            assert!(report.contains(&path.display().to_string()), "{report}");
        }
        assert_eq!(session.marks().len(), 2);
        assert!(session.link.is_none());
        assert!(window.marks().is_empty());
        session.review().expect_err("keep the window where it is");
        assert!(window.asked().iter().all(|request| matches!(
            request,
            Request::Hello { .. } | Request::State
        )));

        let scan = session
            .prepare_scan(&ScanParams {
                path: Some(temp.path().to_path_buf()),
                ..ScanParams::default()
            })
            .expect("explicit scan covers both marks");
        assert!(matches!(scan, ScanPlan::Window { .. }));
        assert_eq!(session.marks().len(), 2);
        assert_eq!(window.marks().len(), 2);
        let asked = window.asked();
        let scan_at = asked
            .iter()
            .position(|request| matches!(request, Request::Scan { .. }))
            .expect("explicit scan");
        let mark_at = asked
            .iter()
            .position(|request| matches!(request, Request::Mark { .. }))
            .expect("handover");
        assert!(scan_at < mark_at, "{asked:?}");
        assert!(matches!(
            &asked[mark_at],
            Request::Mark { root, targets }
                if root == temp.path() && targets.len() == 2
        ));
        session.review().expect("review the handed-over marks");
    }

    #[cfg(unix)]
    #[test]
    fn a_scan_that_does_not_cover_earlier_marks_keeps_them_here() {
        let temp = fixture();
        let socket = temp.path().join("window.sock");
        let mut session = Session::detached(socket.clone(), None);
        scanned(&mut session, temp.path());
        let path = temp.path().join("Downloads");
        session
            .mark(&PathsParams {
                paths: vec![path.clone()],
            })
            .expect("mark offline");

        let window = FakeWindow::open(&socket, &temp.path().join("notes"));
        for root in [temp.path().join("src"), path.clone()] {
            let error = session
                .prepare_scan(&ScanParams {
                    path: Some(root),
                    ..ScanParams::default()
                })
                .expect_err("the mark must be inside the requested root");
            assert!(error.contains("containing all marked paths"), "{error}");
            assert_eq!(session.marks()[0].path, path);
            assert!(session.link.is_none());
        }
        assert!(window.asked().iter().all(|request| matches!(
            request,
            Request::Hello { .. } | Request::State
        )));
    }

    #[cfg(unix)]
    #[test]
    fn a_mark_missing_from_the_windows_tree_is_kept_and_explained() {
        let temp = fixture();
        let socket = temp.path().join("window.sock");
        let mut session = Session::detached(socket.clone(), None);
        scanned(&mut session, temp.path());
        let path = temp.path().join("notes/todo.txt");
        session
            .mark(&PathsParams {
                paths: vec![path.clone(), temp.path().join("Downloads")],
            })
            .expect("mark offline");
        std::fs::remove_file(&path).expect("remove fixture file");

        let window = FakeWindow::open(&socket, temp.path());
        let status = session.status();
        assert!(status.contains("not in the window's tree"), "{status}");
        assert!(status.contains("they are kept here"), "{status}");
        assert_eq!(session.marks().len(), 2);
        assert_eq!(window.marks().len(), 1, "partial handover is reported");
        assert!(session.link.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn unmark_after_a_partial_handover_clears_both_copies() {
        for clear_all in [false, true] {
            let temp = fixture();
            let socket = temp.path().join("window.sock");
            let mut session = Session::detached(socket.clone(), None);
            scanned(&mut session, temp.path());
            let missing = temp.path().join("notes/todo.txt");
            let accepted = temp.path().join("Downloads");
            session
                .mark(&PathsParams {
                    paths: vec![missing.clone(), accepted.clone()],
                })
                .expect("mark offline");
            std::fs::remove_file(&missing).expect("remove fixture file");

            let window = FakeWindow::open(&socket, temp.path());
            let unrelated = temp.path().join(".cache");
            Session::detached(socket, None)
                .mark(&PathsParams {
                    paths: vec![unrelated.clone()],
                })
                .expect("mark in the window separately");
            session.status();
            assert_eq!(window.marks().len(), 2, "partial handover");
            assert_eq!(session.marks().len(), 2);

            let report = session
                .unmark(&PathsParams {
                    paths: if clear_all { vec![] } else { vec![accepted] },
                })
                .expect("unmark accepted paths in both places");
            if clear_all {
                assert!(window.marks().is_empty());
                assert!(report.starts_with("Nothing is marked."), "{report}");
                assert!(session.marks().is_empty());
            } else {
                assert_eq!(window.marks().len(), 1);
                assert_eq!(window.marks()[0].path, unrelated);
                assert!(report.starts_with("Marked 1"), "{report}");
                assert_eq!(session.marks()[0].path, missing);
                // The other mark still cannot be handed over. Clearing it
                // later must not bring the accepted path back on reconnect.
                session
                    .unmark(&PathsParams::default())
                    .expect("clear remaining mark");
            }
            session.status();
            assert!(session.marks().is_empty());
            assert!(window.marks().is_empty());
            assert!(window.asked().iter().any(|request| {
                matches!(request, Request::Unmark { paths } if paths.is_empty())
            }));
        }
    }

    #[cfg(unix)]
    #[test]
    fn review_launches_one_window_covering_every_offline_scan_root() {
        let temp = fixture();
        let socket = temp.path().join("window.sock");
        let windows = Arc::new(Mutex::new(Vec::new()));
        let launched = Arc::clone(&windows);
        let expected = temp.path().to_path_buf();
        let launch_socket = socket.clone();
        let launcher = Box::new(move |root: &Path| {
            assert_eq!(root, expected, "launch on the common ancestor");
            launched
                .lock()
                .expect("windows")
                .push(FakeWindow::open(&launch_socket, root));
            Ok(())
        });
        let mut session = Session::detached(socket, Some(launcher));
        scanned(&mut session, &temp.path().join(".cache"));
        scanned(&mut session, &temp.path().join("src"));
        session
            .mark(&PathsParams {
                paths: vec![
                    temp.path().join(".cache/pip"),
                    temp.path().join("src/app/target"),
                ],
            })
            .expect("mark offline");

        let review = session.review().expect("launch and review both marks");
        assert!(review.contains("2 marked"), "{review}");
        assert!(session.link.is_some());
        let (marks, asked) = {
            let windows = windows.lock().expect("windows");
            assert_eq!(windows.len(), 1);
            (windows[0].marks(), windows[0].asked())
        };
        assert_eq!(marks.len(), 2);
        assert!(
            asked
                .iter()
                .any(|request| matches!(request, Request::Review))
        );
        assert!(asked.iter().all(|request| match request {
            Request::Mark { root, targets } => {
                root == temp.path() && targets.len() == 2
            }
            Request::Scan { .. } | Request::Show { .. } => false,
            _ => true,
        }));
    }

    /// A window that will not take the marks made before it linked is not
    /// followed: the marks stay here, and the agent is told why instead of
    /// losing them.
    #[cfg(any(unix, windows))]
    #[test]
    fn marks_a_window_refuses_are_kept_and_explained() {
        let temp = fixture();
        let socket = endpoint(temp.path());
        let mut session = Session::detached(socket.clone(), None);
        scanned(&mut session, temp.path());
        session
            .mark(&PathsParams {
                paths: vec![temp.path().join("Downloads")],
            })
            .expect("mark offline");

        let _window = FakeWindow::removing(&socket, temp.path());
        let status = session.status();
        assert!(status.contains("a removal is running"), "{status}");
        assert_eq!(session.marks().len(), 1, "the mark is kept");
        let review = session.review().expect_err("not followed");
        assert!(review.contains("did not take the marks"), "{review}");
    }

    /// A listing is answered even when the window cannot follow it.
    #[cfg(any(unix, windows))]
    #[test]
    fn ls_with_show_keeps_the_listing_when_the_window_cannot_follow() {
        let temp = fixture();
        let socket = endpoint(temp.path());
        let _window =
            FakeWindow::showing(&socket, temp.path(), Some("build-box"));
        let mut session = Session::detached(socket, None);
        scanned(&mut session, temp.path());
        let listing = session
            .ls(&LsParams {
                path: temp.path().join("src"),
                limit: None,
                show: true,
            })
            .expect("the listing stands");
        assert!(listing.contains("app/"), "{listing}");
        assert!(listing.contains("Window: not shown."), "{listing}");
    }

    /// A window showing an SSH host holds another machine's paths: the
    /// session works on this computer without it, never hands it a mark,
    /// and says why when asked to show or review.
    #[cfg(any(unix, windows))]
    #[test]
    fn a_window_on_an_ssh_host_is_not_driven() {
        let temp = fixture();
        let socket = endpoint(temp.path());
        let window =
            FakeWindow::showing(&socket, temp.path(), Some("build-box"));
        let mut session = Session::detached(socket, None);

        let status = session.status();
        assert!(status.contains("SSH host build-box"), "{status}");

        // Reads and marks fall back to this session's own scan.
        scanned(&mut session, temp.path());
        let plan = session
            .mark(&PathsParams {
                paths: vec![temp.path().join(".cache")],
            })
            .expect("mark here");
        assert!(plan.starts_with("Marked 1"), "{plan}");
        assert!(plan.contains("SSH host build-box"), "{plan}");
        assert_eq!(session.marks().len(), 1);

        let review = session.review().expect_err("no review on a host");
        assert!(review.contains("SSH host build-box"), "{review}");
        assert!(review.contains("This computer"), "{review}");
        let shown = session
            .show_path(&ShowParams {
                path: temp.path().join("src"),
            })
            .expect_err("no show on a host");
        assert!(shown.contains("SSH host build-box"), "{shown}");

        assert!(
            window.asked().iter().all(|request| matches!(
                request,
                Request::Hello { .. } | Request::State
            )),
            "only hello and state reach a window on a host: {:?}",
            window.asked()
        );
        assert!(window.marks().is_empty());
    }

    #[test]
    fn client_names_read_like_products() {
        let temp = fixture();
        let mut session = unlinked(temp.path());
        session.set_client("claude-code");
        assert_eq!(session.client(), "Claude Code");
        session.set_client("codex-cli");
        assert_eq!(session.client(), "Codex");
        session.set_client("claude-ai");
        assert_eq!(session.client(), "Claude");
        session.set_client("zed");
        assert_eq!(session.client(), "zed");
        session.set_client("");
        assert_eq!(session.client(), "an agent");
    }
}

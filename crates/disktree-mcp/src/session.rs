//! What one connected agent knows: the window it is driving, or failing
//! that its own scans, and the marks.
//!
//! When a disktree window is open it is the source of truth: `ls`, `find`
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
use disktree_core::removal::{Target, normalize, plan};
use disktree_core::scan::{ScanHandle, ScanOptions};
use disktree_core::size::{human_bytes, human_count, share};
use disktree_core::space::{
    SpaceInfo, device_for, space_info, volume_root_for, volumes,
};
use disktree_core::tree::{Node, path_of};
use disktree_link::{
    FindRequest, FoundList, Insight, Link, Listing, MarkRequest, Request,
    Response, ScanOptionsRequest, Space, State, TreeSummary, VERSION,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::report::{age, children, describe, display, matches};

/// How many findings a scan report lists.
const INSIGHTS: usize = 8;
/// Top-level rows a scan report shows.
const TOP: usize = 12;
/// Rows a listing shows unless asked otherwise.
const DEFAULT_LIMIT: usize = 20;
/// The most rows any listing shows: past this the agent should filter.
const MAX_LIMIT: usize = 200;
/// How long to wait for a window that was just launched to start listening.
const LAUNCH_WAIT: Duration = Duration::from_secs(20);
/// A window that did not answer is not launched again sooner than this: it
/// may still be coming up, and a second one would only compete for the
/// socket.
const RELAUNCH_AFTER: Duration = Duration::from_mins(1);

/// Unix seconds now.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs().cast_signed())
}

/// Starts a disktree window on a root, for the review screen when none is
/// open. Swapped out in tests.
pub type Launcher = Box<dyn Fn(&Path) -> io::Result<()> + Send>;

/// One finished scan of this process's own, kept for the rest of the
/// session.
#[derive(Debug)]
pub struct Scan {
    pub root: PathBuf,
    pub tree: Arc<Node>,
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

/// What `disktree_scan` takes.
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

/// What `disktree_ls` takes.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LsParams {
    /// The directory to list, under the scanned root.
    pub path: PathBuf,
    /// How many entries to list, largest first. Default 20, at most 200.
    pub limit: Option<usize>,
    /// Also move the disktree window there, so the user sees what you see.
    #[serde(default)]
    pub show: bool,
}

/// What kind of entry `disktree_find` matches; [`Kind`] as a parameter.
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

/// A kind of data, as `disktree_find` filters it; [`Category`] as a
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

/// What `disktree_find` takes.
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
    /// Only space that can be had back: caches, build output, package
    /// stores, installed dependencies, sync history, trash, scratch.
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

/// What `disktree_mark` and `disktree_unmark` take.
#[derive(Clone, Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PathsParams {
    /// Absolute paths under the scanned root. For `disktree_unmark`, an
    /// empty list unmarks everything.
    #[serde(default)]
    pub paths: Vec<PathBuf>,
}

/// What `disktree_show` takes.
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
    /// When a window was last started from here, so one that is slow to
    /// come up is not started twice.
    launched: Option<Instant>,
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

/// Start a disktree window on `root`, detached from this process so the
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
        Self::detached(
            disktree_link::socket_path(),
            Some(Box::new(launch_window)),
        )
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
            launched: None,
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
        if !self.try_connect() {
            return Ok(ScanPlan::Local { root, options });
        }
        let request = Request::Scan {
            root: root.clone(),
            options: ScanOptionsRequest {
                apparent_size: Some(options.apparent_size),
                include_hidden: Some(options.include_hidden),
                one_filesystem: Some(options.one_filesystem),
                max_depth: Some(options.max_depth),
            },
        };
        match self.call(&request)? {
            Response::Error { message } => Err(message),
            _ => Ok(ScanPlan::Window { root }),
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
        if tree.unreadable > 0 {
            let _ = write!(out, " \u{00b7} {} unreadable", tree.unreadable);
        }
        if tree.apparent_size {
            out.push_str(" \u{00b7} apparent sizes");
        }
        if !tree.include_hidden {
            out.push_str(" \u{00b7} hidden entries skipped");
        }
        out.push('\n');
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
            "Next: disktree_ls goes into a directory, disktree_find lists \
             the largest files or every cache, disktree_mark marks what \
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
                "Nothing is scanned yet. Call disktree_scan first; with no \
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
        let (crumbs, node) = scan.tree.descend(relative).ok_or_else(|| {
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
        if path.starts_with(&state.root) {
            return Ok(state.root.clone());
        }
        Err(format!(
            "{} is outside the disktree window's tree, which is rooted at \
             {}. The window is what the user sees, so it is what you read: \
             call disktree_scan on a directory containing it and the window \
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
            found.scan.tree.bytes,
            &found.path,
            found.node,
            limit,
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
        out.push_str(&children(&listing, now));
        if params.show {
            out.push_str(&self.show_in_window(&listing.root, &path)?);
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
        let scope: Vec<(PathBuf, &Node)> = if let Some(path) = &request.path {
            let located = self.locate(path)?;
            vec![(located.path, located.node)]
        } else {
            if self.scans.is_empty() {
                return Err(
                    "Nothing is scanned yet. Call disktree_scan first."
                        .to_string(),
                );
            }
            self.scans
                .iter()
                .map(|scan| (scan.root.clone(), &*scan.tree))
                .collect()
        };
        let mut all = FoundList::default();
        for (path, node) in &scope {
            let found =
                FoundList::of(&query::find(path, node, &filter, request.limit));
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
        Ok(out)
    }

    // ── the window ──────────────────────────────────────────────────────

    /// Whether the window link could be opened without launching anything.
    /// A new link learns the window's state and is handed the marks made
    /// here before it existed; from then on the window's marks are the
    /// marks.
    fn try_connect(&mut self) -> bool {
        if self.link.is_some() {
            return true;
        }
        let Ok(link) = Link::connect_at(&self.socket) else {
            return false;
        };
        self.link = Some(link);
        // `hello` adopts the window's marks, which would lose these.
        let mine = std::mem::take(&mut self.marks);
        let request = Request::Hello {
            client: self.client.clone(),
            version: VERSION,
        };
        if self.call(&request).is_err() {
            self.marks = mine;
            return false;
        }
        if self.push_marks(&mine).is_err() {
            return false;
        }
        true
    }

    /// Open the link, launching a window on `root` when none answers.
    fn ensure_link(&mut self, root: &Path) -> Result<(), String> {
        if self.try_connect() {
            return Ok(());
        }
        let Some(launcher) = &self.launcher else {
            return Err(format!(
                "No disktree window is open, and this session cannot open \
                 one here ({}). Start `disktree {}` yourself, then call \
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
                "A disktree window was started {} s ago and has not \
                 answered yet. Give it a moment and call again; if none \
                 appeared, start `disktree {}` by hand.",
                launched.elapsed().as_secs(),
                root.display()
            ));
        }
        launcher(root).map_err(|error| {
            format!("could not start a disktree window: {error}")
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
        Err("A disktree window was started but did not answer in time. \
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
                    self.adopt(state);
                }
                Ok(response)
            }
            Err(error) => {
                self.link = None;
                self.window = None;
                Err(format!("the disktree window went away: {error}"))
            }
        }
    }

    /// The window's marks are the marks: keep ours in step.
    fn adopt(&mut self, state: &State) {
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
        for root in self.roots_of(mine) {
            let targets: Vec<MarkRequest> = mine
                .iter()
                .filter(|target| target.path.starts_with(&root))
                .map(|target| MarkRequest {
                    path: target.path.clone(),
                    bytes: target.bytes,
                    is_dir: target.is_dir,
                })
                .collect();
            self.call(&Request::Mark {
                root: root.clone(),
                targets,
            })?;
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
        match &self.window {
            Some(state) => {
                let tree = match &state.tree {
                    Some(tree) => format!(
                        " ({} \u{00b7} {} files \u{00b7} scanned {})",
                        human_bytes(tree.bytes),
                        human_count(tree.files),
                        age(now(), tree.scanned_at)
                    ),
                    None => String::new(),
                };
                format!(
                    "Window: open on {}{tree}, {} screen{}{}. Its tree is \
                     what disktree_ls and disktree_find read.",
                    self.show(&state.root),
                    state.screen,
                    if state.scanning { ", scanning" } else { "" },
                    match state.marks.len() {
                        0 => String::new(),
                        1 => ", 1 mark".to_string(),
                        n => format!(", {n} marks"),
                    },
                )
            }
            None if self.launcher.is_some() => {
                "Window: none open. disktree_review opens one on the marks \
                 for the user to confirm; disktree_show opens one on a path."
                    .to_string()
            }
            None => "Window: none open, and none can be opened from here."
                .to_string(),
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
        if self.try_connect() {
            self.call(&Request::Unmark { paths })?;
        } else if paths.is_empty() {
            self.marks.clear();
        } else {
            self.marks.retain(|target| !paths.contains(&target.path));
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
        let mut out = String::new();
        if self.marks.is_empty() {
            out.push_str("Nothing is marked.\n");
            out.push_str(&self.window_line());
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
        for root in self.roots_of(&marks) {
            let mine: Vec<Target> = marks
                .iter()
                .filter(|mark| mark.path.starts_with(&root))
                .cloned()
                .collect();
            out.push_str(&self.plan_for(&mine, &root));
        }
        out.push_str(&self.window_line());
        out
    }

    /// The plan for the marks under one root: targets, what another mark
    /// covers, what the guards keep back, and the space it frees.
    fn plan_for(&self, mine: &[Target], root: &Path) -> String {
        let mut out = String::new();
        let plan = plan(mine, root);
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
        match space_info(root).ok() {
            Some(space) => {
                let after = space.after_removing(plan.reclaim());
                let _ = writeln!(
                    out,
                    "Frees {} on {}: {} free now, {} after.",
                    human_bytes(plan.reclaim()),
                    device_for(root).unwrap_or_else(|| self.show(root)),
                    human_bytes(space.available),
                    human_bytes(after.available),
                );
            }
            None => {
                let _ = writeln!(
                    out,
                    "Frees {} on the volume of {}.",
                    human_bytes(plan.reclaim()),
                    self.show(root)
                );
            }
        }
        if plan.foreign() > 0 {
            let _ = writeln!(
                out,
                "Plus {} on other volumes, not counted above.",
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
            return Err("Nothing is marked. disktree_mark some paths first."
                .to_string());
        }
        let marks = self.marks.clone();
        let root = self
            .roots_of(&marks)
            .into_iter()
            .next()
            .ok_or("no root for the marks")?;
        self.ensure_link(&root)?;
        if let Response::Error { message } = self.call(&Request::Review)? {
            return Err(message);
        }
        let _ = self.call(&Request::State);
        let total: u64 = self.marks.iter().map(|mark| mark.bytes).sum();
        Ok(format!(
            "The review screen is open in disktree: {} marked, {} to free. \
             The user decides there. Enter moves the marks to the trash, \
             where they can be recovered; permanent deletion is a choice \
             away and always asks first. Afterwards disktree_status reports \
             what was actually freed.",
            self.marks.len(),
            human_bytes(total)
        ))
    }

    // ── status ──────────────────────────────────────────────────────────

    /// Everything this session knows, in a screenful.
    pub fn status(&mut self) -> String {
        let now = now();
        let mut out = String::new();
        let _ = writeln!(
            out,
            "disktree {} serving {} over MCP. The agent marks; the person \
             removes on the review screen.",
            env!("CARGO_PKG_VERSION"),
            self.client
        );
        out.push_str(&self.window_line());
        out.push('\n');
        if self.scans.is_empty() {
            if self.link.is_none() {
                out.push_str(
                    "Scans: none yet. disktree_scan with no arguments scans \
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
                    human_bytes(scan.tree.bytes),
                    human_count(scan.tree.files),
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
        if self.marks.is_empty() {
            out.push_str("Marked: nothing.\n");
        } else {
            let total: u64 = self.marks.iter().map(|mark| mark.bytes).sum();
            let _ = writeln!(
                out,
                "Marked: {} \u{00b7} {} (disktree_mark with no new paths, or \
                 disktree_unmark, shows the plan)",
                self.marks.len(),
                human_bytes(total)
            );
        }
        out.push_str(&volumes_report());
        out
    }
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
    node: &'a Node,
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
                bytes: scan.tree.bytes,
                files: scan.tree.files,
                dirs: u64::from(scan.tree.dirs),
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
                scan.tree.bytes,
                &scan.root,
                &scan.tree,
                TOP,
            ),
            insights: scan
                .insights
                .iter()
                .map(|candidate| Insight {
                    path: path_of(&scan.root, &scan.tree, &candidate.crumbs),
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
        "the window answered with something unexpected ({}); restart it",
        match response {
            Response::State(_) => "state",
            Response::Marked { .. } => "marked",
            Response::Listing(_) => "listing",
            Response::Found(_) => "found",
            Response::Ok => "ok",
            Response::Error { .. } => "error",
        }
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
    let insights = worth_a_look(&tree, at, INSIGHTS);
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
    #[cfg(unix)]
    use disktree_link::{Incoming, Listener, Mark, Refused, Removal};
    #[cfg(unix)]
    use std::sync::Mutex;
    #[cfg(unix)]
    use std::sync::atomic::{AtomicBool, Ordering};
    #[cfg(unix)]
    use std::thread;

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
    #[cfg(unix)]
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
        Session::detached(dir.join("no-window.sock"), None)
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
        assert!(report.contains("Next: disktree_ls"), "{report}");
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
        assert!(missing.contains("disktree_scan"), "{missing}");

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
        assert!(plan.contains("Frees"), "{plan}");
        assert!(plan.contains("Window: none open, and none can be opened"));
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

    /// A stand-in for the window: holds a real tree of the fixture, answers
    /// over the real socket the way the app does, and records what it was
    /// asked.
    #[cfg(unix)]
    struct FakeWindow {
        listener: Arc<Listener>,
        asked: Arc<Mutex<Vec<Request>>>,
        marks: Arc<Mutex<Vec<Mark>>>,
        stop: Arc<AtomicBool>,
    }

    #[cfg(unix)]
    struct FakeTree {
        root: PathBuf,
        tree: Arc<Node>,
        marks: Vec<Mark>,
    }

    #[cfg(unix)]
    impl FakeTree {
        fn state(&self, screen: &str) -> State {
            State {
                version: VERSION,
                root: self.root.clone(),
                current: self.root.clone(),
                screen: screen.into(),
                scanning: false,
                scanned: true,
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
                tree: Some(TreeSummary {
                    bytes: self.tree.bytes,
                    files: self.tree.files,
                    dirs: u64::from(self.tree.dirs),
                    scanned_at: now() - 30,
                    elapsed_ms: 1_500,
                    unreadable: 0,
                    apparent_size: true,
                    include_hidden: true,
                }),
                insights: vec![Insight {
                    path: self.root.join(".cache"),
                    bytes: 400_000,
                    what: "reclaimable: cache".into(),
                }],
            }
        }

        fn answer(&mut self, request: &Request) -> Response {
            match request {
                Request::Hello { .. } | Request::State => {
                    Response::State(self.state("explore"))
                }
                Request::Scan { root, .. } => {
                    assert_eq!(*root, self.root, "the fake has one tree");
                    Response::State(self.state("explore"))
                }
                Request::Ls { path, limit } => {
                    let Some((_, node)) = path
                        .strip_prefix(&self.root)
                        .ok()
                        .and_then(|relative| self.tree.descend(relative))
                    else {
                        return Response::error("not in the window's tree");
                    };
                    Response::Listing(Listing::of(
                        &self.root,
                        self.tree.bytes,
                        path,
                        node,
                        *limit,
                    ))
                }
                Request::Find(request) => {
                    let filter = match request.filter() {
                        Ok(filter) => filter,
                        Err(message) => return Response::error(message),
                    };
                    let found = query::find(
                        &self.root,
                        &self.tree,
                        &filter,
                        request.limit,
                    );
                    Response::Found(FoundList::of(&found))
                }
                Request::Mark { targets, .. } => {
                    let mut refused = Vec::new();
                    for target in targets {
                        let node =
                            target.path.strip_prefix(&self.root).ok().and_then(
                                |relative| self.tree.descend(relative),
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
            }
        }
    }

    #[cfg(unix)]
    impl FakeWindow {
        fn open(socket: &Path, root: &Path) -> Self {
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
            let mut fake = FakeTree {
                root: root.to_path_buf(),
                tree: Arc::new(
                    disktree_core::scan::scan(root, apparent()).expect("scan"),
                ),
                marks: Vec::new(),
            };
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

    #[cfg(unix)]
    impl Drop for FakeWindow {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            let _ = &self.listener;
        }
    }

    #[cfg(unix)]
    #[test]
    fn with_a_window_open_its_tree_is_what_the_agent_reads() {
        let temp = fixture();
        let socket = temp.path().join("window.sock");
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
        assert!(outside.contains("disktree_scan"), "{outside}");

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

    #[cfg(unix)]
    #[test]
    fn with_a_window_open_marks_live_there_and_review_opens_it() {
        let temp = fixture();
        let socket = temp.path().join("window.sock");
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
        assert!(shown.contains("Window: showing"), "{shown}");

        let review = session.review().expect("review");
        assert!(review.contains("review screen is open"), "{review}");

        let asked = window.asked();
        assert!(
            asked
                .iter()
                .any(|request| matches!(request, Request::Show { .. }))
        );
        assert!(
            asked
                .iter()
                .any(|request| matches!(request, Request::Review))
        );
    }

    #[cfg(unix)]
    #[test]
    fn marks_made_before_the_window_opened_are_handed_over() {
        let temp = fixture();
        let socket = temp.path().join("window.sock");
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

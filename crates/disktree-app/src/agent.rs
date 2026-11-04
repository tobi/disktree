//! The window's end of the agent link.
//!
//! `pwrdisk mcp` scans and marks on its own; this is how an open window
//! follows along. A listener on the per-user socket queues what an agent
//! asks, the UI thread answers between frames, and every answer carries the
//! window's state, so the agent's view of the marks is always the window's.
//!
//! While a window is open it is the agent's source of truth: `ls`, `find`
//! and `scan` are answered from the tree on screen, and a mark is measured
//! from it too, so the agent and the person read the same numbers.
//!
//! An agent can look, mark and open review. Destructive requests require a
//! single-use intent matching an explicitly confirmed preview. UI and MCP
//! removal share the same guards and configured capacity limit.

use disktree_core::path_style::PathStyle;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use disktree_core::query;
use disktree_core::removal::{Plan, RemovalMode, Target};
use disktree_core::tree::{Node, path_of};
use disktree_link::{
    FindRequest, FoundList, Incoming, Insight, Listener, Listing, Mark,
    MarkRequest, Refused, Removal, Request, Response, ScanOptionsRequest,
    SelectionEstimate, Space, State, TreeSummary, VERSION,
};
use gpui_kit::Context;

use crate::marks::{display_path, is_hidden};
use crate::state::{Disktree, Screen, now_seconds};

/// How often the UI thread looks for agent requests. Short enough that a
/// mark lands in the mosaic before the agent's next sentence.
const POLL: Duration = Duration::from_millis(50);

/// Who is connected and what they last did, for the header and the panel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Presence {
    pub client: String,
    pub action: String,
    pub at: Instant,
}

/// The listener and what has come over it.
#[derive(Debug)]
pub struct AgentLink {
    listener: Listener,
    pub presence: Option<Presence>,
    /// A path to show once the scan an agent asked for lands.
    pending_show: Option<PathBuf>,
    /// The last removal, for an agent that asks what was freed.
    pub last_removal: Option<Removal>,
    pub(crate) removal_preview: Option<crate::agent_removal::PreparedRemoval>,
    pub(crate) removal_operation: Option<disktree_link::RemovalOperation>,
    pub(crate) removal_operation_epoch: u64,
}

impl AgentLink {
    pub const fn new(listener: Listener) -> Self {
        Self {
            listener,
            presence: None,
            pending_show: None,
            last_removal: None,
            removal_preview: None,
            removal_operation: None,
            removal_operation_epoch: 0,
        }
    }
}

/// The screen as the protocol names it.
const fn screen_name(screen: Screen) -> &'static str {
    match screen {
        Screen::Explore => "explore",
        Screen::Review => "review",
        Screen::Running => "running",
        Screen::Done => "done",
    }
}

/// One way to connect a client: an id for its Copy button, what it is
/// called, and the text to paste.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connect {
    pub id: &'static str,
    pub label: &'static str,
    pub text: String,
}

/// The ways to connect a client, with the binary where it is: the bare
/// command line first, for any client that asks for a command, then the
/// one-liners for the clients that have one. The executable's own path, so
/// the command works from a bundle, a symlink on `PATH`, or a build
/// directory alike.
pub fn connect_commands(exe: &Path) -> Vec<Connect> {
    let exe = exe.display();
    vec![
        Connect {
            id: "agents-copy-command",
            label: "Command line",
            text: format!("\"{exe}\" mcp"),
        },
        Connect {
            id: "agents-copy-claude",
            label: "Claude Code",
            text: format!(
                "claude mcp add --scope user pwrdisk -- \"{exe}\" mcp"
            ),
        },
        Connect {
            id: "agents-copy-codex",
            label: "Codex",
            text: format!("codex mcp add pwrdisk -- \"{exe}\" mcp"),
        },
        Connect {
            id: "agents-copy-json",
            label: "Other MCP clients (JSON)",
            text: format!(
                "{{\"mcpServers\": {{\"pwrdisk\": {{\"command\": \"{exe}\", \
                 \"args\": [\"mcp\"]}}}}}}"
            ),
        },
    ]
}

/// What to ask an agent once it is connected. It asks for a proposal, not
/// a standing instruction: the skill marks without asking only when told
/// to.
pub const FIRST_PROMPT: &str = "What is eating my disk? Use PwrDisk to \
find out, and show me what could go before you mark anything.";

fn presence_line(presence: Option<&Presence>) -> String {
    presence.map_or_else(
        || "No agent is connected to this window.".to_owned(),
        |presence| {
            format!(
                "{} is connected \u{00b7} {}",
                presence.client, presence.action
            )
        },
    )
}

impl Disktree {
    /// Start answering agents on `listener`.
    pub fn attach_agent(&mut self, listener: Listener, cx: &Context<'_, Self>) {
        self.agent = Some(AgentLink::new(listener));
        Self::poll_agent(cx);
    }

    fn poll_agent(cx: &Context<'_, Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL).await;
                let keep_going =
                    this.update(cx, Self::poll_agent_once).unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    /// Answer everything that arrived. Returns whether there is a listener
    /// to keep polling. Driven by the timer above, and by the tests.
    pub(crate) fn poll_agent_once(
        &mut self,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        let Some(agent) = &self.agent else {
            return false;
        };
        // Every noted action stamps the presence, so its time says whether
        // what Settings shows can have changed, without formatting it.
        let before = agent.presence.as_ref().map(|presence| presence.at);
        let incoming = agent.listener.drain();
        if incoming.is_empty() {
            self.forget_departed_agent(cx);
            self.tell_settings(before, cx);
            return true;
        }
        for item in incoming {
            match item {
                Incoming::Request { request, reply, .. } => {
                    let response = self.answer_agent(&request, cx);
                    reply.send(response);
                }
                Incoming::Closed { .. } => {}
            }
        }
        self.forget_departed_agent(cx);
        self.tell_settings(before, cx);
        cx.notify();
        true
    }

    /// Settings › Agents (MCP) shows who is connected: redraw it when that
    /// line changes, and only then, since this runs every [`POLL`].
    fn tell_settings(
        &self,
        before: Option<Instant>,
        cx: &mut Context<'_, Self>,
    ) {
        let now = self
            .agent
            .as_ref()
            .and_then(|agent| agent.presence.as_ref())
            .map(|presence| presence.at);
        if now != before {
            cx.defer(crate::settings::agents_changed);
        }
    }

    /// Whether an agent is connected, and who it is and what it last did,
    /// as Settings says it.
    pub fn agent_status(&self) -> (bool, String) {
        match &self.agent {
            Some(agent) => (
                agent.presence.is_some(),
                presence_line(agent.presence.as_ref()),
            ),
            None => (
                false,
                "This window cannot be driven here; an agent still scans \
                 and marks on its own."
                    .to_owned(),
            ),
        }
    }

    /// Once the last connection is gone, so is the presence.
    fn forget_departed_agent(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(agent) = &mut self.agent
            && agent.presence.is_some()
            && agent.listener.connections() == 0
        {
            agent.presence = None;
            cx.notify();
        }
    }

    pub(crate) fn note_agent(&mut self, client: Option<&str>, action: String) {
        let Some(agent) = &mut self.agent else {
            return;
        };
        let client = client
            .map(str::to_owned)
            .or_else(|| agent.presence.as_ref().map(|p| p.client.clone()))
            .unwrap_or_else(|| "an agent".to_string());
        agent.presence = Some(Presence {
            client,
            action,
            at: Instant::now(),
        });
    }

    pub(crate) fn answer_agent(
        &mut self,
        request: &Request,
        cx: &mut Context<'_, Self>,
    ) -> Response {
        // A window on an SSH host holds that host's paths. An agent's paths
        // are this computer's, and a mark or a scan would be read against
        // the wrong machine; it only learns where the window is.
        if let Some(host) = &self.remote
            && !matches!(request, Request::Hello { .. } | Request::State)
        {
            return Response::error(disktree_link::remote_refusal(host));
        }
        match request {
            Request::Hello { client, version } => {
                if *version != VERSION {
                    return Response::error(format!(
                        "PwrDisk GUI {}: {}",
                        env!("CARGO_PKG_VERSION"),
                        disktree_link::protocol_mismatch(VERSION, *version),
                    ));
                }
                if let Some(agent) = &mut self.agent {
                    agent.removal_preview = None;
                }
                self.note_agent(Some(client), "connected".to_string());
                Response::State(self.agent_state())
            }
            Request::State => Response::State(self.agent_state()),
            Request::PrepareRemoval { mode } => {
                self.agent_prepare_removal(*mode)
            }
            Request::Remove(request) => self.agent_remove(request, cx),
            Request::Show { root, path } => self.agent_show(root, path, cx),
            Request::Scan { root, options } => {
                self.agent_scan(root, options, cx)
            }
            Request::Ls { path, limit } => self.agent_ls(path, *limit),
            Request::Find(request) => self.agent_find(request),
            Request::Estimate {
                paths,
                peer_scope,
                peer_limit,
            } => self.agent_estimate(paths, peer_scope.as_deref(), *peer_limit),
            Request::Mark { root, targets } => {
                self.agent_marks(root, targets, cx)
            }
            Request::Unmark { paths } => {
                if self.screen == Screen::Running {
                    return Response::error("a removal is running");
                }
                if paths.is_empty() {
                    self.clear_marks(cx);
                } else {
                    for path in paths {
                        self.unmark(path, cx);
                    }
                }
                self.leave_review_if_empty();
                self.note_agent(None, "unmarked".to_string());
                Response::State(self.agent_state())
            }
            Request::Review => {
                if self.screen == Screen::Running {
                    return Response::error("a removal is running");
                }
                if self.marks.is_empty() {
                    return Response::error("nothing is marked");
                }
                self.reread_disks();
                self.screen = Screen::Review;
                self.confirm_open = false;
                self.note_agent(None, "opened the review screen".to_string());
                cx.notify();
                Response::Ok
            }
        }
    }

    /// Scan `root` with the options the agent gave, or rescan it. Answered
    /// at once; the agent polls the state until `scanning` turns false.
    fn agent_scan(
        &mut self,
        root: &Path,
        options: &ScanOptionsRequest,
        cx: &mut Context<'_, Self>,
    ) -> Response {
        if self.screen == Screen::Running {
            return Response::error("a removal is running");
        }
        if let Some(apparent) = options.apparent_size {
            self.options.apparent_size = apparent;
        }
        if let Some(hidden) = options.include_hidden {
            self.options.include_hidden = hidden;
        }
        if let Some(one) = options.one_filesystem {
            self.options.one_filesystem = one;
        }
        if let Some(depth) = options.max_depth {
            self.options.max_depth = depth;
        }
        self.note_agent(
            None,
            format!(
                "scanning {}",
                display_path(root, self.home.as_deref(), PathStyle::Native)
            ),
        );
        if root == self.root_path {
            self.screen = Screen::Explore;
            self.start_scan(cx);
        } else {
            self.agent_set_root(root, cx);
        }
        cx.notify();
        Response::State(self.agent_state())
    }

    /// Scan `root` for an agent. A window still on the source chooser
    /// leaves it first: the agent's request names the root explicitly,
    /// and `start_scan` would otherwise only reopen the chooser.
    fn agent_set_root(&mut self, root: &Path, cx: &mut Context<'_, Self>) {
        if self.awaiting_root {
            self.awaiting_root = false;
            self.close_volumes(cx);
        }
        self.set_root(root.to_path_buf(), cx);
    }

    /// The node at `path` in the tree on screen, or why there is none.
    fn agent_node(&self, path: &Path) -> Result<Node<'_>, String> {
        let tree = self.tree.as_deref().ok_or_else(|| {
            if self.scan.is_some() {
                "the window is still scanning; ask again when the state \
                 says scanning is false"
            } else {
                "the window has no tree"
            }
            .to_string()
        })?;
        let relative = path.strip_prefix(&self.root_path).map_err(|_| {
            format!(
                "{} is outside the window's tree, rooted at {}",
                display_path(path, self.home.as_deref(), PathStyle::Native),
                display_path(
                    &self.root_path,
                    self.home.as_deref(),
                    PathStyle::Native
                )
            )
        })?;
        tree.root()
            .descend(relative)
            .map(|(_, node)| node)
            .ok_or_else(|| {
                format!(
                    "{} is not in the window's tree: it may be new, hidden, \
                 below the depth limit, or on another filesystem",
                    display_path(path, self.home.as_deref(), PathStyle::Native)
                )
            })
    }

    /// Estimate from the complete window tree without changing any state.
    fn agent_estimate(
        &self,
        paths: &[PathBuf],
        peer_scope: Option<&Path>,
        peer_limit: usize,
    ) -> Response {
        if self.scan.is_some() || self.screen == Screen::Running {
            return Response::error(
                "Wait for the scan or removal to finish before estimating.",
            );
        }
        let Some(tree) = self.tree.as_deref() else {
            return Response::error("The window has no completed scan.");
        };
        // The tree still holds what was removed since the scan, at the
        // scan's sizes: an estimate over it would price space already
        // gone.
        if let Some(path) = paths.iter().find(|path| {
            self.was_removed(path)
                || self.removed.below(path, PathStyle::Native) > 0
        }) {
            return Response::error(format!(
                "{} {} since the scan; rescan before estimating it.",
                display_path(path, self.home.as_deref(), PathStyle::Native),
                if self.was_removed(path) {
                    "was removed"
                } else {
                    "lost part of itself to a removal"
                },
            ));
        }
        let summary = self.agent_state().tree;
        let context = disktree_core::planning::Context::of(
            &self.root_path,
            &self.options,
            summary.as_ref().map_or(0, |tree| tree.scanned_at),
            summary.as_ref().map_or(0, |tree| tree.unreadable),
        );
        match disktree_core::planning::estimate(
            tree, context, paths, peer_scope, peer_limit,
        ) {
            Ok(report) => Response::Estimate(report),
            Err(message) => Response::error(message),
        }
    }

    /// What is in a directory of the tree on screen.
    fn agent_ls(&mut self, path: &Path, limit: usize) -> Response {
        let mut listing = match self.agent_node(path) {
            Ok(node) => Listing::of(
                &self.root_path,
                self.tree.as_ref().map_or(0, |tree| tree.root().bytes),
                path,
                node,
                limit,
            ),
            Err(message) => return Response::error(message),
        };
        self.note_removal(&mut listing.entry, path);
        for child in &mut listing.children {
            let path = path.join(&child.name);
            self.note_removal(child, &path);
        }
        self.note_agent(
            None,
            format!(
                "reading {}",
                display_path(path, self.home.as_deref(), PathStyle::Native)
            ),
        );
        Response::Listing(listing)
    }

    /// The largest entries of the tree on screen matching a description.
    fn agent_find(&mut self, request: &FindRequest) -> Response {
        let filter = match request.filter() {
            Ok(filter) => filter,
            Err(message) => return Response::error(message),
        };
        let from = request
            .path
            .clone()
            .unwrap_or_else(|| self.root_path.clone());
        let mut found = match self.agent_node(&from) {
            Ok(node) => {
                FoundList::of(&query::find(&from, node, &filter, request.limit))
            }
            Err(message) => return Response::error(message),
        };
        for item in &mut found.matches {
            self.note_removal(&mut item.entry, &item.path);
        }
        self.note_agent(None, "searching the tree".to_string());
        Response::Found(found)
    }

    /// Say what a removal since the scan took of `path`: the tree keeps
    /// it until a rescan, and its size is still the scan's.
    fn note_removal(&self, entry: &mut disktree_link::Entry, path: &Path) {
        if self.removed.is_empty() {
            return;
        }
        entry.removed = self.was_removed(path);
        if !entry.removed {
            entry.removed_bytes = self.removed.below(path, PathStyle::Native);
        }
    }

    /// Mark what the agent asks, measured from the tree on screen. A path
    /// the tree does not have is refused, not guessed at.
    fn agent_marks(
        &mut self,
        root: &Path,
        targets: &[MarkRequest],
        cx: &mut Context<'_, Self>,
    ) -> Response {
        if self.screen == Screen::Running {
            return Response::error("a removal is running");
        }
        if root != self.root_path {
            self.agent_set_root(root, cx);
        }
        let mut marked = 0_usize;
        let mut refused = Vec::new();
        for target in targets {
            // Without a tree yet, the agent's own measurement stands in;
            // `Marks::refresh` corrects it when the scan lands.
            if self.was_removed(&target.path) {
                refused.push(Refused {
                    path: target.path.clone(),
                    reason: "removed since the scan; it is still listed \
                             until a rescan"
                        .into(),
                });
                continue;
            }
            let measured = if self.tree.is_some() {
                match self.agent_node(&target.path) {
                    // A directory part of which was removed frees only
                    // the rest.
                    Ok(node) => (
                        node.bytes.saturating_sub(
                            self.removed.below(&target.path, PathStyle::Native),
                        ),
                        node.is_dir(),
                    ),
                    Err(reason) => {
                        refused.push(Refused {
                            path: target.path.clone(),
                            reason,
                        });
                        continue;
                    }
                }
            } else {
                (target.bytes, target.is_dir)
            };
            match self.agent_mark(Target {
                hidden: is_hidden(&target.path, PathStyle::Native),
                path: target.path.clone(),
                bytes: measured.0,
                is_dir: measured.1,
            }) {
                Ok(()) => marked += 1,
                Err(reason) => refused.push(Refused {
                    path: target.path.clone(),
                    reason,
                }),
            }
        }
        self.leave_review_if_empty();
        self.note_agent(
            None,
            format!(
                "marked {marked} path{}",
                if marked == 1 { "" } else { "s" }
            ),
        );
        cx.notify();
        Response::Marked {
            state: self.agent_state(),
            refused,
        }
    }

    /// A review screen over nothing is an empty list; go back to the tree.
    fn leave_review_if_empty(&mut self) {
        if self.screen == Screen::Review && self.marks.is_empty() {
            self.screen = Screen::Explore;
        }
    }

    /// Mark `target` for an agent: like a press of Space, except it never
    /// unmarks, and a path inside a marked directory is left alone rather
    /// than toggled. Err with the reason when it is not marked afterwards:
    /// the guards refuse it, or it would take the list past the limit,
    /// which only the person can go past.
    fn agent_mark(&mut self, target: Target) -> Result<(), String> {
        if self.marks.contains(&target.path, PathStyle::Native) {
            return Ok(());
        }
        if !target.path.starts_with(&self.root_path)
            || target.path == self.root_path
        {
            return Err("the scanned root itself, or outside it".into());
        }
        if self.marked_ancestor(&target.path).is_some() {
            return Ok(());
        }
        self.refusal_cache.replace(None);
        if let Some(reason) = self.agent_mark_refusal(&target) {
            return Err(reason);
        }
        self.space_baseline = self.space_baseline.or(self.space);
        let path = target.path.clone();
        self.marks.toggle(target, PathStyle::Native);
        // A directory's mark absorbs the marks inside it, as Space does.
        let inside: Vec<PathBuf> = self
            .marks
            .items()
            .iter()
            .filter(|item| item.path != path && item.path.starts_with(&path))
            .map(|item| item.path.clone())
            .collect();
        for inner in &inside {
            self.marks.remove(inner, PathStyle::Native);
        }
        Ok(())
    }

    /// Show `path`, rescanning from `root` first when the window is
    /// elsewhere. A directory is opened; a file is selected in its own.
    fn agent_show(
        &mut self,
        root: &Path,
        path: &Path,
        cx: &mut Context<'_, Self>,
    ) -> Response {
        if self.screen == Screen::Running {
            return Response::error("a removal is running");
        }
        self.note_agent(
            None,
            format!(
                "looking at {}",
                display_path(path, self.home.as_deref(), PathStyle::Native)
            ),
        );
        if root != self.root_path {
            self.agent_set_root(root, cx);
        }
        if self.scan.is_some() || self.tree.is_none() {
            // The tree is on its way; show the path when it lands.
            if let Some(agent) = &mut self.agent {
                agent.pending_show = Some(path.to_path_buf());
            }
            return Response::Ok;
        }
        if self.show_path(path, cx) {
            Response::Ok
        } else {
            Response::error(format!(
                "{} is not in the window's tree",
                display_path(path, self.home.as_deref(), PathStyle::Native)
            ))
        }
    }

    /// Put `path` on screen. Returns whether it was found.
    fn show_path(&mut self, path: &Path, cx: &mut Context<'_, Self>) -> bool {
        let Some(crumbs) = self.crumbs_for_path(path) else {
            return false;
        };
        if matches!(self.screen, Screen::Review | Screen::Done) {
            self.screen = Screen::Explore;
        }
        let is_dir = self.node_at(&crumbs).is_some_and(Node::is_dir);
        if is_dir {
            self.go_to(crumbs, cx);
        } else {
            self.reveal(crumbs, cx);
        }
        true
    }

    /// Called when a scan lands: show what an agent asked for meanwhile.
    pub(crate) fn finish_pending_show(&mut self, cx: &mut Context<'_, Self>) {
        let pending = self
            .agent
            .as_mut()
            .and_then(|agent| agent.pending_show.take());
        if let Some(path) = pending {
            self.show_path(&path, cx);
        }
    }

    /// Called when a removal finishes: keep what it did for the agent.
    pub(crate) fn record_removal(&mut self) {
        let Some(agent) = &mut self.agent else {
            return;
        };
        let totals = self.run_list.totals();
        // Use the same measurement and per-run baseline as the ledger.
        let observed = self
            .ledger
            .cohorts()
            .last()
            .and_then(crate::ledger::Cohort::observed);
        agent.last_removal = Some(Removal {
            mode: match self.removal_mode {
                RemovalMode::Trash => "trash".to_string(),
                RemovalMode::Permanent => "permanent".to_string(),
            },
            removed: totals.removed,
            failed: totals.failed_all(),
            bytes: totals.reported_done,
            freed: observed.map(|delta| delta.max(0).unsigned_abs()),
            observed,
            at: now_seconds(),
        });
    }

    /// The removals since the scan on screen, for the agent: what it needs
    /// to say how much a run of batches freed without a rescan.
    fn agent_ledger(&self) -> Option<disktree_link::RemovalLedger> {
        let cohorts = self.ledger.since(self.scans_done);
        let totals = crate::ledger::Ledger::totals(cohorts)?;
        let seconds = |at: std::time::SystemTime| {
            at.duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_secs().cast_signed())
        };
        Some(disktree_link::RemovalLedger {
            cohorts: cohorts
                .iter()
                .map(|cohort| disktree_link::RemovalCohort {
                    mode: match cohort.mode {
                        RemovalMode::Trash => "trash".to_string(),
                        RemovalMode::Permanent => "permanent".to_string(),
                    },
                    removed: cohort.removed,
                    failed: cohort.failed,
                    reported: cohort.reported,
                    private: cohort.private,
                    observed: cohort.observed(),
                    started: seconds(cohort.started),
                    finished: seconds(cohort.finished),
                })
                .collect(),
            reported: totals.reported,
            private: totals.private,
            observed: totals.observed,
        })
    }

    /// What the window shows, for the agent.
    pub fn agent_state(&self) -> State {
        State {
            version: VERSION,
            app_version: env!("CARGO_PKG_VERSION").into(),
            root: self.root_path.clone(),
            current: self.current_path(),
            screen: screen_name(self.screen).to_string(),
            scanning: self.scan.is_some(),
            scanned: self.tree.is_some(),
            marks: self
                .marks
                .items()
                .iter()
                .map(|target| Mark {
                    path: target.path.clone(),
                    bytes: target.bytes,
                    is_dir: target.is_dir,
                })
                .collect(),
            estimate: (self.tree.is_some()
                && self.scan.is_none()
                && self.remote.is_none())
            .then(|| self.agent_selection_estimate(&self.plan())),
            space: self.space.map(|space| Space {
                total: space.total,
                free: space.free,
                available: space.available,
            }),
            removal: self
                .agent
                .as_ref()
                .and_then(|agent| agent.last_removal.clone()),
            removal_operation: self
                .agent
                .as_ref()
                .and_then(|agent| agent.removal_operation.clone()),
            removals: self.agent_ledger(),
            tree: self.tree.as_deref().map(|tree| TreeSummary {
                bytes: tree.root().bytes,
                files: tree.root().files,
                dirs: u64::from(tree.root().dirs),
                scanned_at: self.scanned_at,
                elapsed_ms: self.scan_elapsed.map_or(0, |elapsed| {
                    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
                }),
                unreadable: self.progress.errors,
                apparent_size: self.options.apparent_size,
                include_hidden: self.options.include_hidden,
            }),
            insights: self
                .tree
                .as_deref()
                .map(|tree| {
                    self.insights
                        .iter()
                        .map(|candidate| Insight {
                            path: path_of(
                                &self.root_path,
                                tree.root(),
                                &candidate.crumbs,
                                PathStyle::Native,
                            ),
                            bytes: candidate.bytes,
                            what: candidate.finding.describe(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            remote: self.remote.clone(),
        }
    }

    pub(crate) fn agent_selection_estimate(
        &self,
        plan: &Plan,
    ) -> SelectionEstimate {
        // Apparent lengths cannot project physical savings, and disabling
        // APFS metadata cannot establish that all listed blocks are private.
        if self.options.apparent_size
            || (cfg!(target_os = "macos") && !self.options.apfs_clone_metadata)
        {
            return SelectionEstimate::unknown(plan);
        }
        let mut estimate = SelectionEstimate::of(plan);
        if let Some(tree) = &self.tree {
            let mut resolver = crate::marks::Resolver::new(
                &self.root_path,
                tree.root(),
                PathStyle::Native,
            );
            let nodes: Vec<Node<'_>> = plan
                .targets
                .iter()
                .filter_map(|target| resolver.resolve(&target.path))
                .map(|(_, node)| node)
                .collect();
            estimate = estimate.with_shared(nodes);
        }
        estimate.partial |=
            self.progress.errors > 0 || self.options.max_depth.is_some();
        estimate
    }
}

//! The window's end of the agent link.
//!
//! `disktree mcp` scans and marks on its own; this is how an open window
//! follows along. A listener on the per-user socket queues what an agent
//! asks, the UI thread answers between frames, and every answer carries the
//! window's state, so the agent's view of the marks is always the window's.
//!
//! While a window is open it is the agent's source of truth: `ls`, `find`
//! and `scan` are answered from the tree on screen, and a mark is measured
//! from it too, so the agent and the person read the same numbers.
//!
//! An agent can look, mark and open the review screen. It cannot remove
//! anything: the review screen is where the person decides, same as when
//! they marked everything themselves.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use disktree_core::query;
use disktree_core::removal::{RemovalMode, Target};
use disktree_core::space::space_info;
use disktree_core::tree::{Node, path_of};
use disktree_link::{
    FindRequest, FoundList, Incoming, Insight, Listener, Listing, Mark,
    MarkRequest, Refused, Removal, Request, Response, ScanOptionsRequest,
    Space, State, TreeSummary, VERSION,
};
use gpui_kit::Context;
use gpui_omarchy::Status;

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
}

impl AgentLink {
    pub const fn new(listener: Listener) -> Self {
        Self {
            listener,
            presence: None,
            pending_show: None,
            last_removal: None,
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

/// The commands that connect each client, with the binary where it is.
/// The executable's own path, so the command works from a bundle, a
/// symlink on `PATH`, or a build directory alike.
pub fn connect_commands(exe: &Path) -> Vec<(&'static str, String)> {
    let exe = exe.display();
    vec![
        (
            "Claude Code",
            format!("claude mcp add --scope user disktree -- \"{exe}\" mcp"),
        ),
        ("Codex", format!("codex mcp add disktree -- \"{exe}\" mcp")),
        (
            "Other MCP clients",
            format!(
                "{{\"mcpServers\": {{\"disktree\": {{\"command\": \"{exe}\", \
                 \"args\": [\"mcp\"]}}}}}}"
            ),
        ),
    ]
}

/// What to ask an agent once it is connected.
pub const FIRST_PROMPT: &str =
    "What is eating my disk? Use disktree to find out and mark what can go.";

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
        let incoming = agent.listener.drain();
        if incoming.is_empty() {
            self.forget_departed_agent(cx);
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
        cx.notify();
        true
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

    fn note_agent(&mut self, client: Option<&str>, action: String) {
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

    fn answer_agent(
        &mut self,
        request: &Request,
        cx: &mut Context<'_, Self>,
    ) -> Response {
        match request {
            Request::Hello { client, version } => {
                if *version != VERSION {
                    return Response::error(format!(
                        "this window speaks link version {VERSION}, the \
                         agent version {version}; restart the older one"
                    ));
                }
                self.note_agent(Some(client), "connected".to_string());
                Response::State(self.agent_state())
            }
            Request::State => Response::State(self.agent_state()),
            Request::Show { root, path } => self.agent_show(root, path, cx),
            Request::Scan { root, options } => {
                self.agent_scan(root, options, cx)
            }
            Request::Ls { path, limit } => self.agent_ls(path, *limit),
            Request::Find(request) => self.agent_find(request),
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
            format!("scanning {}", display_path(root, self.home.as_deref())),
        );
        if root == self.root_path {
            self.screen = Screen::Explore;
            self.start_scan(cx);
        } else {
            self.set_root(root.to_path_buf(), cx);
        }
        cx.notify();
        Response::State(self.agent_state())
    }

    /// The node at `path` in the tree on screen, or why there is none.
    fn agent_node(&self, path: &Path) -> Result<&Node, String> {
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
                display_path(path, self.home.as_deref()),
                display_path(&self.root_path, self.home.as_deref())
            )
        })?;
        tree.descend(relative).map(|(_, node)| node).ok_or_else(|| {
            format!(
                "{} is not in the window's tree: it may be new, hidden, \
                 below the depth limit, or on another filesystem",
                display_path(path, self.home.as_deref())
            )
        })
    }

    /// What is in a directory of the tree on screen.
    fn agent_ls(&mut self, path: &Path, limit: usize) -> Response {
        let listing = match self.agent_node(path) {
            Ok(node) => Listing::of(
                &self.root_path,
                self.tree.as_ref().map_or(0, |tree| tree.bytes),
                path,
                node,
                limit,
            ),
            Err(message) => return Response::error(message),
        };
        self.note_agent(
            None,
            format!("reading {}", display_path(path, self.home.as_deref())),
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
        let found = match self.agent_node(&from) {
            Ok(node) => {
                FoundList::of(&query::find(&from, node, &filter, request.limit))
            }
            Err(message) => return Response::error(message),
        };
        self.note_agent(None, "searching the tree".to_string());
        Response::Found(found)
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
            self.set_root(root.to_path_buf(), cx);
        }
        let mut marked = 0_usize;
        let mut refused = Vec::new();
        for target in targets {
            // Without a tree yet, the agent's own measurement stands in;
            // `Marks::refresh` corrects it when the scan lands.
            let measured = if self.tree.is_some() {
                match self.agent_node(&target.path) {
                    Ok(node) => (node.bytes, node.is_dir()),
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
            if self.agent_mark(Target {
                hidden: is_hidden(&target.path),
                path: target.path.clone(),
                bytes: measured.0,
                is_dir: measured.1,
            }) {
                marked += 1;
            } else {
                refused.push(Refused {
                    path: target.path.clone(),
                    reason: "the scanned root itself, or outside it".into(),
                });
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
    /// than toggled. Returns whether the path is marked afterwards.
    fn agent_mark(&mut self, target: Target) -> bool {
        if self.marks.contains(&target.path) {
            return true;
        }
        if !target.path.starts_with(&self.root_path)
            || target.path == self.root_path
        {
            return false;
        }
        if self.marked_ancestor(&target.path).is_some() {
            return true;
        }
        self.space_baseline = self.space_baseline.or(self.space);
        let path = target.path.clone();
        self.marks.toggle(target);
        // A directory's mark absorbs the marks inside it, as Space does.
        let inside: Vec<PathBuf> = self
            .marks
            .items()
            .iter()
            .filter(|item| item.path != path && item.path.starts_with(&path))
            .map(|item| item.path.clone())
            .collect();
        for inner in &inside {
            self.marks.remove(inner);
        }
        true
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
            format!("looking at {}", display_path(path, self.home.as_deref())),
        );
        if root != self.root_path {
            self.set_root(root.to_path_buf(), cx);
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
                display_path(path, self.home.as_deref())
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
        let freed = space_info(&self.root_path)
            .ok()
            .zip(self.space_baseline)
            .map(|(now, before)| {
                now.available.saturating_sub(before.available)
            });
        agent.last_removal = Some(Removal {
            mode: match self.removal_mode {
                RemovalMode::Trash => "trash".to_string(),
                RemovalMode::Permanent => "permanent".to_string(),
            },
            removed: usize::try_from(self.run_summary.removed)
                .unwrap_or(usize::MAX),
            failed: self.run_summary.failed,
            bytes: self.run_summary.bytes,
            freed,
            at: now_seconds(),
        });
    }

    /// What the window shows, for the agent.
    pub fn agent_state(&self) -> State {
        State {
            version: VERSION,
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
            space: self.space.map(|space| Space {
                total: space.total,
                free: space.free,
                available: space.available,
            }),
            removal: self
                .agent
                .as_ref()
                .and_then(|agent| agent.last_removal.clone()),
            tree: self.tree.as_deref().map(|tree| TreeSummary {
                bytes: tree.bytes,
                files: tree.files,
                dirs: u64::from(tree.dirs),
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
                                tree,
                                &candidate.crumbs,
                            ),
                            bytes: candidate.bytes,
                            what: candidate.finding.describe(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// Copy a connect command, and say so.
    pub fn copy_connect_text(
        &mut self,
        text: String,
        what: &str,
        cx: &mut Context<'_, Self>,
    ) {
        cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text));
        self.notice = Some((format!("copied {what}"), Status::Success));
        cx.notify();
    }
}

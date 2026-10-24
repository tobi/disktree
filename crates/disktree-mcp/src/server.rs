//! The MCP surface: eight tools, one prompt, one resource.
//!
//! Each tool is a thin call into [`Session`]; the words an agent reads come
//! from there. Tools that only look are marked read-only, so a client can
//! run them without asking. Nothing here removes a file: marking is not
//! destructive, and the review screen belongs to the person.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use rmcp::handler::server::router::prompt::PromptRouter;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    GetPromptResult, Implementation, ListResourcesResult, PromptMessage,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    Resource, ResourceContents, Role, ServerCapabilities, ServerConfig,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::{
    ErrorData, ServerHandler, prompt, prompt_handler, prompt_router, tool,
    tool_handler, tool_router,
};

use crate::session::{
    FindParams, LsParams, PathsParams, ScanParams, ScanPlan, Session,
    ShowParams, run_scan,
};

/// How often a scan the window is running is looked in on.
const SCAN_POLL: Duration = Duration::from_millis(500);
/// How long a window scan is followed before the agent is told to come
/// back: a whole disk can take minutes, a tool call should not.
const SCAN_WAIT: Duration = Duration::from_mins(10);

/// The free-space procedure's address as a resource.
pub const SKILL_URI: &str = "disktree://skills/free-space";

/// What the client is told at `initialize`: enough to use the tools well
/// without reading the skill first.
const INSTRUCTIONS: &str = "\
disktree measures a directory tree the way the disk does: allocated \
blocks, hardlinks once, hidden entries included. Start with \
disktree_status, then disktree_scan (no arguments: the home directory; \
disk: true: the whole volume). Walk with disktree_ls, largest first; \
disktree_find lists the largest files or every cache, build directory \
and package store at once. disktree_mark marks paths for removal and \
returns the plan: what the guards keep back and why, and the free space \
it would give back on the volume. You never delete: disktree_review \
opens disktree's review screen, where the user presses Enter to move \
the marks to the trash. disktree_status afterwards says what was freed. \
When a disktree window is open its tree is what you read: ls, find and \
scan are answered from what the user sees, and a path outside its root \
needs a disktree_scan from higher up, which the window follows. Pass \
show: true to disktree_ls or call disktree_show to move the view. The \
prompt free-space and the resource disktree://skills/free-space hold \
the full procedure.";

/// The server: one session behind a lock, so the tools can run from any
/// protocol task while scans run off to the side.
#[derive(Clone)]
pub struct Server {
    session: Arc<Mutex<Session>>,
    tool_router: ToolRouter<Self>,
    prompt_router: PromptRouter<Self>,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").finish_non_exhaustive()
    }
}

impl Server {
    pub fn new(session: Session) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            tool_router: Self::tool_router(),
            prompt_router: Self::prompt_router(),
        }
    }

    /// Run `f` with the session. A poisoned lock means a tool panicked; the
    /// state is still worth more than nothing.
    pub fn with<R>(&self, f: impl FnOnce(&mut Session) -> R) -> R {
        let mut session =
            self.session.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut session)
    }
}

#[tool_router]
impl Server {
    /// What this session knows: scans held, the window if one is open, what
    /// is marked, the last removal, and every volume's free space. Start
    /// here.
    #[tool(
        name = "disktree_status",
        annotations(title = "disktree status", read_only_hint = true)
    )]
    async fn status(&self) -> String {
        self.with(Session::status)
    }

    /// Scan a directory tree and keep the result for the other tools. With
    /// no arguments it scans the home directory, which takes seconds; disk:
    /// true scans the whole volume. Returns totals, the volume's free space
    /// and how much of its used space this scan explains, the top level
    /// largest first, and the biggest things that could plausibly go.
    /// Scanning the same root again replaces the earlier scan. When a
    /// disktree window is open it does the scanning and shows the result;
    /// the window's tree is then what every other tool reads.
    #[tool(
        name = "disktree_scan",
        annotations(title = "Scan a directory", read_only_hint = true)
    )]
    async fn scan(
        &self,
        Parameters(params): Parameters<ScanParams>,
    ) -> Result<String, String> {
        match self.with(|s| s.prepare_scan(&params))? {
            ScanPlan::Local { root, options } => {
                let scan = tokio::task::spawn_blocking(move || {
                    run_scan(root, options)
                })
                .await
                .map_err(|error| format!("the scan task failed: {error}"))??;
                Ok(self.with(|s| s.store_scan(scan)))
            }
            ScanPlan::Window { root } => {
                // The window walks; the session is unlocked between looks
                // so disktree_status still answers meanwhile.
                let started = Instant::now();
                loop {
                    if let Some(report) = self.with(|s| s.window_scan(&root))? {
                        return Ok(report);
                    }
                    if started.elapsed() > SCAN_WAIT {
                        return Err(format!(
                            "The window is still scanning {} after {} s. \
                             disktree_status says when it is done; then \
                             disktree_ls reads the tree.",
                            root.display(),
                            started.elapsed().as_secs()
                        ));
                    }
                    tokio::time::sleep(SCAN_POLL).await;
                }
            }
        }
    }

    /// List a scanned directory, largest first: each entry's share, size,
    /// kind of data, whether its space can be had back and why, and when it
    /// was last written. show: true moves the disktree window there.
    #[tool(
        name = "disktree_ls",
        annotations(title = "List a directory", read_only_hint = true)
    )]
    async fn ls(
        &self,
        Parameters(params): Parameters<LsParams>,
    ) -> Result<String, String> {
        self.with(|s| s.ls(&params))
    }

    /// The largest entries matching the filters, anywhere beneath a path or
    /// every scanned root: files over a size, everything reclaimable
    /// (caches, build output, package stores), one kind of data, a name,
    /// or what nobody has written to in a while. A matching directory is
    /// listed once, without its contents.
    #[tool(
        name = "disktree_find",
        annotations(title = "Find the largest matches", read_only_hint = true)
    )]
    async fn find(
        &self,
        Parameters(params): Parameters<FindParams>,
    ) -> Result<String, String> {
        self.with(|s| s.find(&params))
    }

    /// Show a path in the disktree window, selected in its directory, so
    /// the user sees what you are looking at. Opens a window when none is.
    #[tool(
        name = "disktree_show",
        annotations(title = "Show in the window", read_only_hint = true)
    )]
    async fn show(
        &self,
        Parameters(params): Parameters<ShowParams>,
    ) -> Result<String, String> {
        self.with(|s| s.show_path(&params))
    }

    /// Mark paths for removal. Nothing is deleted: marks appear in the
    /// disktree window when one is open, and the user removes them from the
    /// review screen. Returns the plan: what is marked, what the guards
    /// keep back and why (the home directory, a mount point, a system
    /// tree), what is covered by another mark, and the free space it gives
    /// back on the volume.
    #[tool(
        name = "disktree_mark",
        annotations(
            title = "Mark for removal",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn mark(
        &self,
        Parameters(params): Parameters<PathsParams>,
    ) -> Result<String, String> {
        self.with(|s| s.mark(&params))
    }

    /// Unmark paths, or everything when no paths are given. Returns the
    /// plan that remains.
    #[tool(
        name = "disktree_unmark",
        annotations(
            title = "Unmark",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true
        )
    )]
    async fn unmark(
        &self,
        Parameters(params): Parameters<PathsParams>,
    ) -> Result<String, String> {
        self.with(|s| s.unmark(&params))
    }

    /// Open disktree's review screen over the marks, starting a window if
    /// none is open. The user decides there: Enter moves the marks to the
    /// trash; permanent deletion always asks first. Call `disktree_status`
    /// afterwards for what was freed.
    #[tool(
        name = "disktree_review",
        annotations(
            title = "Open the review screen",
            read_only_hint = false,
            destructive_hint = false
        )
    )]
    async fn review(&self) -> Result<String, String> {
        self.with(Session::review)
    }
}

#[prompt_router]
impl Server {
    /// How to free space with disktree: measure, scan, walk, sweep,
    /// classify, mark, and hand the decision back on the review screen.
    #[prompt(name = "free-space")]
    async fn free_space(&self) -> GetPromptResult {
        GetPromptResult::new(vec![PromptMessage::new_text(
            Role::User,
            crate::SKILL,
        )])
        .with_description("Free up disk space with disktree")
    }
}

#[tool_handler(router = self.tool_router)]
#[prompt_handler(router = self.prompt_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_prompts()
            .enable_resources()
            .build();
        ServerConfig::new(capabilities)
            .with_server_info(
                Implementation::new("disktree", env!("CARGO_PKG_VERSION"))
                    .with_title("disktree")
                    .with_website_url("https://github.com/tobi/disktree"),
            )
            .with_instructions(INSTRUCTIONS)
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        if let Some(info) = context.peer.peer_info() {
            let name = info.client_info.name.clone();
            self.with(|s| s.set_client(&name));
        }
    }

    async fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let resource = Resource::new(SKILL_URI, "free-space")
            .with_title("Free up space with disktree")
            .with_description(
                "The procedure: measure, scan, walk, sweep, classify, mark, \
                 and hand the decision back on the review screen.",
            )
            .with_mime_type("text/markdown");
        Ok(ListResourcesResult::with_all_items(vec![resource]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        if request.uri != SKILL_URI {
            return Err(ErrorData::resource_not_found(
                format!("no resource at {}", request.uri),
                None,
            ));
        }
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            crate::SKILL,
            SKILL_URI,
        )])
        .into())
    }
}

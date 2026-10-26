//! `disktree mcp`: the reports as Model Context Protocol tools, over stdio.
//!
//! Newline-delimited JSON-RPC 2.0 on stdin and stdout; stdout carries
//! protocol messages and nothing else, and diagnostics go to stderr. Every
//! tool is read-only and says so in its annotations: the server lists
//! files and directories and can do nothing else to them.
//!
//! A scan of a home directory takes a while, so the last one is kept for
//! [`CACHE_FOR`] and reused by any tool asking the same question of the
//! same tree (`refresh` forces a new walk). While a scan runs the server
//! still reads stdin, so a client's cancellation stops the walk, and a
//! client that sent a progress token is told how far it has got.

use std::collections::{HashSet, VecDeque};
use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::VERSION;
use crate::query::{
    self, DEFAULT_TOP, MAX_TOP, Query, ScanSettings, Sections, check_depth,
    check_top, normalize_extension, parse_category, parse_older_than,
    parse_reclaim, parse_size,
};
use crate::report::{self, Scanned};

/// How long a finished scan answers further calls.
pub const CACHE_FOR: Duration = Duration::from_mins(5);

/// Entries `list_files` returns unless asked otherwise: it is for filtered
/// lists, which are often longer than a top 20.
const LIST_FILES_TOP: usize = 100;

/// Protocol revisions this server speaks, newest first.
const PROTOCOL_VERSIONS: [&str; 4] =
    ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "\
disktree measures disk usage and is strictly read-only: these tools list \
files and directories with their sizes and never delete, move or change \
anything. Every size comes as bytes and human text, with its percent of the \
scanned total and ages in days, so no arithmetic is needed. Start with \
scan_path for an overview or reclaimable_space for what can be freed, then \
largest_dirs, largest_files or list_files with filters (min_size, \
extensions, older_than, category, reclaim) for specifics. In largest_dirs, \
nested_in names a listed folder that already contains an entry: do not add \
those together. Scans of large trees take time; results are cached for \
five minutes per path and options, and refresh=true rescans. Sizes are \
on-disk allocation unless apparent_size=true.";

/// JSON-RPC error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Serve on stdin and stdout until stdin closes. Returns the exit status.
pub fn serve_stdio() -> i32 {
    let (sender, inbox) = mpsc::channel();
    let reader = std::thread::Builder::new()
        .name("disktree-mcp-stdin".into())
        .spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
    if let Err(error) = reader {
        eprintln!("disktree mcp: cannot read stdin: {error}");
        return 1;
    }
    eprintln!("disktree mcp {VERSION}: read-only disk usage tools on stdio");
    let stdout = std::io::stdout();
    let mut failed = false;
    let mut server = Server::new(Some(inbox));
    server.run(&mut |message| {
        let mut out = stdout.lock();
        let written = serde_json::to_writer(&mut out, message)
            .map_err(std::io::Error::other)
            .and_then(|()| out.write_all(b"\n"))
            .and_then(|()| out.flush());
        failed |= written.is_err();
    });
    i32::from(failed)
}

/// A scan kept for later calls.
#[derive(Debug)]
struct Cached {
    root: PathBuf,
    settings: ScanSettings,
    at: Instant,
    scan: Arc<Scanned>,
}

/// The protocol state: the inbox, messages read while a scan ran, and the
/// last scan.
#[derive(Debug)]
pub struct Server {
    inbox: Option<Receiver<String>>,
    queued: VecDeque<String>,
    cache: Option<Cached>,
    cache_for: Duration,
    /// Requests cancelled before they were handled, by their id as JSON
    /// text: a cancellation can arrive while the request waits its turn.
    cancelled: HashSet<String>,
    /// The request being handled was cancelled: send it no reply.
    abandoned: bool,
}

impl Server {
    /// A server reading `inbox`; `None` handles only what it is given.
    pub fn new(inbox: Option<Receiver<String>>) -> Self {
        Self {
            inbox,
            queued: VecDeque::new(),
            cancelled: HashSet::new(),
            abandoned: false,
            cache: None,
            cache_for: CACHE_FOR,
        }
    }

    /// Keep scans for `cache_for` instead of [`CACHE_FOR`].
    #[must_use]
    pub const fn with_cache_for(mut self, cache_for: Duration) -> Self {
        self.cache_for = cache_for;
        self
    }

    /// Whether a finished scan is being kept.
    pub const fn has_cached_scan(&self) -> bool {
        self.cache.is_some()
    }

    /// Handle messages until the inbox closes. A cached scan is let go
    /// once it expires, even if no call comes to replace it: a home
    /// directory's tree is the better part of a gigabyte, and a client can
    /// keep the server running for days.
    pub fn run(&mut self, out: &mut dyn FnMut(&Value)) {
        loop {
            let line = if let Some(line) = self.queued.pop_front() {
                line
            } else {
                let Some(inbox) = &self.inbox else { return };
                let expires = self.cache.as_ref().map(|cached| {
                    self.cache_for.saturating_sub(cached.at.elapsed())
                });
                let received = match expires {
                    Some(wait) => inbox.recv_timeout(wait),
                    None => {
                        inbox.recv().map_err(|_| RecvTimeoutError::Disconnected)
                    }
                };
                match received {
                    Ok(line) => line,
                    Err(RecvTimeoutError::Timeout) => {
                        self.cache = None;
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            };
            self.handle_line(&line, out);
        }
    }

    /// Handle one line of input, sending any replies to `out`.
    pub fn handle_line(&mut self, line: &str, out: &mut dyn FnMut(&Value)) {
        if line.trim().is_empty() {
            return;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(message) => self.handle(&message, out),
            Err(error) => out(&failure(
                &Value::Null,
                PARSE_ERROR,
                &format!("not JSON: {error}"),
            )),
        }
    }

    fn handle(&mut self, message: &Value, out: &mut dyn FnMut(&Value)) {
        let Some(object) = message.as_object() else {
            out(&failure(
                &Value::Null,
                INVALID_REQUEST,
                "expected an object",
            ));
            return;
        };
        let id = object.get("id");
        // An id is a string or a number; anything else cannot be echoed.
        if id.is_some_and(|id| !(id.is_string() || id.is_number())) {
            out(&failure(
                &Value::Null,
                INVALID_REQUEST,
                "id must be a string or a number",
            ));
            return;
        }
        // A response to a request this server never sent.
        if object.get("method").is_none()
            && (object.contains_key("result") || object.contains_key("error"))
        {
            return;
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            out(&failure(
                id.unwrap_or(&Value::Null),
                INVALID_REQUEST,
                "no method",
            ));
            return;
        };
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            if let Some(id) = id {
                out(&failure(id, INVALID_REQUEST, "jsonrpc must be \"2.0\""));
            }
            return;
        }
        // Notifications (no id) are never answered; a cancellation is
        // remembered for a request that has not been handled yet.
        let Some(id) = id else {
            if method == "notifications/cancelled"
                && let Some(request) = message["params"].get("requestId")
            {
                self.cancelled.insert(request.to_string());
            }
            return;
        };
        if self.cancelled.remove(&id.to_string()) {
            return;
        }
        let empty = Value::Object(Map::new());
        let params = object.get("params").unwrap_or(&empty);
        let reply = match method {
            "initialize" => Ok(initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => self.call(id, params, out),
            other => Err((METHOD_NOT_FOUND, format!("no method {other}"))),
        };
        // A request cancelled while it ran gets no reply at all.
        if std::mem::take(&mut self.abandoned) {
            return;
        }
        out(&match reply {
            Ok(result) => {
                json!({ "jsonrpc": "2.0", "id": id, "result": result })
            }
            Err((code, message)) => failure(id, code, &message),
        });
    }

    fn call(
        &mut self,
        id: &Value,
        params: &Value,
        out: &mut dyn FnMut(&Value),
    ) -> Result<Value, (i64, String)> {
        let name =
            params.get("name").and_then(Value::as_str).ok_or_else(|| {
                (INVALID_PARAMS, "tools/call needs a tool name".to_owned())
            })?;
        let Some(tool) = Tool::named(name) else {
            return Err((INVALID_PARAMS, format!("no tool named {name}")));
        };
        let empty = Value::Object(Map::new());
        let arguments = params.get("arguments").unwrap_or(&empty);
        let progress_token = params
            .get("_meta")
            .and_then(|meta| meta.get("progressToken"));
        let outcome = self.run_tool(tool, arguments, id, progress_token, out);
        Ok(match outcome {
            Ok(value) => {
                let text = serde_json::to_string_pretty(&value).unwrap_or_else(
                    |error| format!("{{\"error\": \"{error}\"}}"),
                );
                json!({
                    "content": [{ "type": "text", "text": text }],
                    "structuredContent": value,
                    "isError": false,
                })
            }
            Err(message) => json!({
                "content": [{ "type": "text", "text": message }],
                "isError": true,
            }),
        })
    }

    fn run_tool(
        &mut self,
        tool: Tool,
        arguments: &Value,
        id: &Value,
        progress_token: Option<&Value>,
        out: &mut dyn FnMut(&Value),
    ) -> Result<Value, String> {
        let arguments =
            arguments.as_object().ok_or("arguments must be an object")?;
        let allowed = tool.arguments();
        if let Some(unknown) = arguments
            .keys()
            .find(|key| !allowed.contains(&key.as_str()))
        {
            return Err(format!(
                "{} takes no argument {unknown:?}; it takes {}",
                tool.name(),
                allowed.join(", ")
            ));
        }
        let args = Arguments(arguments);
        if tool == Tool::DiskSummary {
            let path = args.path()?.map_or_else(home, Ok)?;
            return report::summary(&query::resolve_root(&path)?);
        }
        let query = query_from(tool, &args)?;
        let refresh = args.flag("refresh")?.unwrap_or(false);
        let scanned = self.scanned(&query, refresh, id, progress_token, out)?;
        report::report(&scanned, &query, chrono::Utc::now().timestamp())
    }

    /// The scan `query` needs: the cached one when it fits, else a new one.
    fn scanned(
        &mut self,
        query: &Query,
        refresh: bool,
        id: &Value,
        progress_token: Option<&Value>,
        out: &mut dyn FnMut(&Value),
    ) -> Result<Arc<Scanned>, String> {
        if !refresh
            && let Some(cached) = &self.cache
            && cached.root == query.root
            && cached.settings == query.settings
            && cached.at.elapsed() < self.cache_for
        {
            return Ok(Arc::clone(&cached.scan));
        }
        // Let the old tree go before walking a new one: a disk's worth of
        // nodes is not small.
        self.cache = None;
        let mut sent = 0_u64;
        let mut progress = |snapshot: &disktree_core::scan::ScanSnapshot| {
            let Some(token) = progress_token else { return };
            let entries = snapshot.files + snapshot.dirs;
            if entries <= sent {
                return;
            }
            sent = entries;
            out(&json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {
                    "progressToken": token,
                    "progress": entries,
                    "message": format!(
                        "scanned {} files in {} directories, {}",
                        snapshot.files,
                        snapshot.dirs,
                        disktree_core::size::human_bytes(snapshot.bytes),
                    ),
                },
            }));
        };
        let mut cancelled = || self.cancelled(id);
        let scanned = report::scan(
            &query.root,
            &query.settings,
            &mut progress,
            &mut cancelled,
        )
        .map_err(|error| {
            format!("cannot scan {}: {error}", query.root.display())
        })?;
        let scanned = Arc::new(scanned);
        self.cache = Some(Cached {
            root: query.root.clone(),
            settings: query.settings.clone(),
            at: Instant::now(),
            scan: Arc::clone(&scanned),
        });
        Ok(scanned)
    }

    /// Read what has arrived while a scan runs: a cancellation of request
    /// `id` stops it, and anything else waits its turn.
    fn cancelled(&mut self, id: &Value) -> bool {
        let Some(inbox) = &self.inbox else {
            return false;
        };
        loop {
            match inbox.try_recv() {
                Ok(line) => {
                    let message = serde_json::from_str::<Value>(&line).ok();
                    let cancels = message.as_ref().and_then(|message| {
                        (message["method"] == "notifications/cancelled")
                            .then(|| message["params"]["requestId"].clone())
                    });
                    match cancels {
                        Some(request) if request == *id => {
                            self.abandoned = true;
                            return true;
                        }
                        // For a request still waiting: skip it when its
                        // turn comes.
                        Some(request) => {
                            self.cancelled.insert(request.to_string());
                        }
                        None => self.queued.push_back(line),
                    }
                }
                // A closed stdin means no more requests, not that the ones
                // already sent are unwanted: `echo … | disktree mcp` still
                // gets its answers.
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                    return false;
                }
            }
        }
    }
}

fn failure(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str);
    let version = asked
        .filter(|asked| PROTOCOL_VERSIONS.contains(asked))
        .unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "disktree",
            "title": "disktree (read-only disk usage)",
            "version": VERSION,
        },
        "instructions": INSTRUCTIONS,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    ScanPath,
    LargestFiles,
    LargestDirs,
    ListFiles,
    ReclaimableSpace,
    DiskSummary,
}

const TOOLS: [Tool; 6] = [
    Tool::ScanPath,
    Tool::LargestFiles,
    Tool::LargestDirs,
    Tool::ListFiles,
    Tool::ReclaimableSpace,
    Tool::DiskSummary,
];

/// Arguments every scanning tool takes.
const SCAN_ARGUMENTS: [&str; 9] = [
    "path",
    "top",
    "depth",
    "apparent_size",
    "one_filesystem",
    "include_hidden",
    "follow_links",
    "exclude",
    "refresh",
];

/// Filters, by whether they apply to files only.
const FILTER_ARGUMENTS: [&str; 5] =
    ["min_size", "older_than", "name", "category", "reclaim"];

impl Tool {
    fn named(name: &str) -> Option<Self> {
        TOOLS.into_iter().find(|tool| tool.name() == name)
    }

    const fn name(self) -> &'static str {
        match self {
            Self::ScanPath => "scan_path",
            Self::LargestFiles => "largest_files",
            Self::LargestDirs => "largest_dirs",
            Self::ListFiles => "list_files",
            Self::ReclaimableSpace => "reclaimable_space",
            Self::DiskSummary => "disk_summary",
        }
    }

    const fn sections(self) -> Sections {
        match self {
            Self::ScanPath | Self::DiskSummary => Sections::ALL,
            Self::ReclaimableSpace => Sections::RECLAIM,
            Self::LargestDirs => Sections::DIRS,
            Self::LargestFiles | Self::ListFiles => Sections::FILES,
        }
    }

    const fn default_top(self) -> usize {
        match self {
            Self::ListFiles => LIST_FILES_TOP,
            _ => DEFAULT_TOP,
        }
    }

    fn arguments(self) -> Vec<&'static str> {
        if self == Self::DiskSummary {
            return vec!["path"];
        }
        // The reclaimable summary describes the whole tree: no listing
        // depth, no filters.
        if self == Self::ReclaimableSpace {
            return SCAN_ARGUMENTS
                .into_iter()
                .filter(|name| *name != "depth")
                .collect();
        }
        let mut names: Vec<&str> = SCAN_ARGUMENTS
            .iter()
            .chain(&FILTER_ARGUMENTS)
            .copied()
            .collect();
        if self.sections().files {
            names.push("extensions");
        }
        names
    }

    const fn description(self) -> &'static str {
        match self {
            Self::ScanPath => {
                "Full overview of a directory (read-only): total, the \
                 volume's free space, breakdowns by top-level folder, by \
                 kind of data and by age (each sums to the total), the \
                 reclaimable summary, and the largest directories and \
                 files. Sizes, percentages and ages are precomputed. \
                 Filters and depth narrow only the two lists."
            }
            Self::LargestFiles => {
                "The largest files beneath a directory, biggest first \
                 (read-only). Filters narrow the list, e.g. min_size=\"1G\" \
                 or extensions=[\"iso\",\"dmg\"]."
            }
            Self::LargestDirs => {
                "The largest directories beneath a directory, biggest first \
                 (read-only). Use depth=1 or 2 for an overview; nested \
                 directories otherwise repeat their parents' space. \
                 category=\"cache\" or reclaim=\"any\" lists the outermost \
                 matching folders, e.g. cache folders."
            }
            Self::ListFiles => {
                "Files matching filters, biggest first, up to top (default \
                 100) (read-only): min_size, extensions, older_than, name \
                 pattern, category, reclaim. The matched totals count every \
                 match, not just those listed."
            }
            Self::ReclaimableSpace => {
                "Space that can likely be freed under a directory \
                 (read-only): caches, build output, reinstallable \
                 dependencies, trash, temporary files. Totals by reason \
                 with what each means, the largest such folders (counted \
                 once, no overlap), and the volume's free space before \
                 and after. Start here for \"what can I clean up\"."
            }
            Self::DiskSummary => {
                "Capacity, used, free and available space of the volume a \
                 path is on. Fast: nothing is scanned (read-only)."
            }
        }
    }

    fn schema(self) -> Value {
        let mut properties = Map::new();
        let mut add = |name: &str, schema: Value| {
            properties.insert(name.to_owned(), schema);
        };
        let strings = |description: &str| {
            json!({
                "anyOf": [
                    { "type": "array", "items": { "type": "string" } },
                    { "type": "string" },
                ],
                "description": description,
            })
        };
        for name in self.arguments() {
            match name {
                "path" => add(
                    name,
                    json!({
                        "type": "string",
                        "description": "Absolute directory path; ~ and ~/… are the \
                            home directory. Defaults to the home directory.",
                    }),
                ),
                "top" => add(
                    name,
                    json!({
                        "type": "integer", "minimum": 1, "maximum": MAX_TOP,
                        "description": format!(
                            "Entries per list (default {}).", self.default_top()
                        ),
                    }),
                ),
                "depth" => add(
                    name,
                    json!({
                        "type": "integer", "minimum": 1,
                        "description": "List only entries at most this many \
                            levels below path; deeper files and directories \
                            are not listed at all, though sizes still include \
                            them. Leave unset to find the largest files \
                            anywhere.",
                    }),
                ),
                "apparent_size" => add(
                    name,
                    json!({
                        "type": "boolean",
                        "description": "Apparent length instead of on-disk \
                            allocation (default false).",
                    }),
                ),
                "one_filesystem" => add(
                    name,
                    json!({
                        "type": "boolean",
                        "description": "Stay on path's volume (default true).",
                    }),
                ),
                "include_hidden" => add(
                    name,
                    json!({
                        "type": "boolean",
                        "description": "Include dotfiles and dot-directories \
                            (default true).",
                    }),
                ),
                "follow_links" => add(
                    name,
                    json!({
                        "type": "boolean",
                        "description": "Follow symlinks (default false).",
                    }),
                ),
                "exclude" => add(
                    name,
                    strings(
                        "Leave these out of the scan: a name pattern \
                     (node_modules, *.vmdk) or, with a /, a path pattern. \
                     * stays within a path component, ** crosses them.",
                    ),
                ),
                "refresh" => add(
                    name,
                    json!({
                        "type": "boolean",
                        "description": "Rescan even if a scan of the same path \
                            and options from the last five minutes is cached.",
                    }),
                ),
                "min_size" => add(
                    name,
                    json!({
                        "type": ["string", "integer"],
                        "description": "At least this size: bytes, or 500M, 1G, \
                            1.5GiB (units are binary).",
                    }),
                ),
                "extensions" => add(
                    name,
                    strings(
                        "Files with one of these extensions (iso, .dmg, tar.gz).",
                    ),
                ),
                "older_than" => add(
                    name,
                    json!({
                        "type": "string",
                        "description": "Last modified before this long ago (30d, \
                            6mo, 1y, 12h) or a date (2025-01-31). For a \
                            directory: nothing beneath it changed since.",
                    }),
                ),
                "name" => add(
                    name,
                    strings(
                        "Name pattern (*backup*, *.log) or, with a /, path pattern.",
                    ),
                ),
                "category" => add(
                    name,
                    json!({
                        "type": "string",
                        "enum": query::category_slugs(),
                        "description": "Kind of data, as disktree classifies it. \
                            Directories: outermost match only.",
                    }),
                ),
                "reclaim" => add(
                    name,
                    json!({
                        "type": "string",
                        "enum": query::reclaim_slugs(),
                        "description": "Space disktree considers reclaimable \
                            (caches, build output, trash...), for this reason or \
                            any. Directories: outermost match only.",
                    }),
                ),
                _ => {}
            }
        }
        let required: Vec<&str> = if self == Self::DiskSummary {
            Vec::new()
        } else {
            vec!["path"]
        };
        json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        })
    }
}

fn tools() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|tool| {
            json!({
                "name": tool.name(),
                "description": tool.description(),
                "inputSchema": tool.schema(),
                "annotations": {
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false,
                },
            })
        })
        .collect()
}

fn home() -> Result<PathBuf, String> {
    std::env::home_dir()
        .ok_or_else(|| "no path given and no home directory".to_owned())
}

/// A tool's arguments, read with type checks.
struct Arguments<'a>(&'a Map<String, Value>);

impl Arguments<'_> {
    fn get(&self, name: &str) -> Option<&Value> {
        self.0.get(name).filter(|value| !value.is_null())
    }

    fn path(&self) -> Result<Option<PathBuf>, String> {
        let Some(value) = self.get("path") else {
            return Ok(None);
        };
        let text = value.as_str().ok_or("path must be a string")?;
        let path = query::expand_home(text);
        if !path.is_absolute() {
            return Err(format!(
                "path must be absolute or start with ~, not {text:?}"
            ));
        }
        Ok(Some(path))
    }

    fn flag(&self, name: &str) -> Result<Option<bool>, String> {
        self.get(name)
            .map(|value| {
                value
                    .as_bool()
                    .ok_or_else(|| format!("{name} must be true or false"))
            })
            .transpose()
    }

    fn whole(&self, name: &str) -> Result<Option<u64>, String> {
        self.get(name)
            .map(|value| {
                value
                    .as_u64()
                    .ok_or_else(|| format!("{name} must be a whole number"))
            })
            .transpose()
    }

    fn text(&self, name: &str) -> Result<Option<&str>, String> {
        self.get(name)
            .map(|value| {
                value
                    .as_str()
                    .ok_or_else(|| format!("{name} must be a string"))
            })
            .transpose()
    }

    /// A list of strings, or one string; for `extensions`, commas split it.
    fn strings(&self, name: &str) -> Result<Vec<String>, String> {
        match self.get(name) {
            None => Ok(Vec::new()),
            Some(Value::String(text)) => Ok(vec![text.clone()]),
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("{name} must be strings"))
                })
                .collect(),
            Some(_) => {
                Err(format!("{name} must be a string or a list of strings"))
            }
        }
    }
}

fn query_from(tool: Tool, args: &Arguments<'_>) -> Result<Query, String> {
    let path = args.path()?.ok_or("path is required")?;
    let mut query = Query::new(query::resolve_root(&path)?);
    query.sections = tool.sections();
    query.top = args
        .whole("top")?
        .map_or_else(|| Ok(tool.default_top()), check_top)?;
    query.depth = args.whole("depth")?.map(check_depth).transpose()?;
    let settings = &mut query.settings;
    if let Some(value) = args.flag("apparent_size")? {
        settings.apparent_size = value;
    }
    if let Some(value) = args.flag("one_filesystem")? {
        settings.one_filesystem = value;
    }
    if let Some(value) = args.flag("include_hidden")? {
        settings.include_hidden = value;
    }
    if let Some(value) = args.flag("follow_links")? {
        settings.follow_links = value;
    }
    settings.exclude = args.strings("exclude")?;
    let filters = &mut query.filters;
    filters.min_size = match args.get("min_size") {
        None => None,
        Some(Value::Number(number)) => Some(number.as_u64().ok_or(
            "min_size must be a whole number of bytes or a size like 1G",
        )?),
        Some(Value::String(text)) => Some(parse_size(text)?),
        Some(_) => {
            return Err(
                "min_size must be a number or a size like 1G".to_owned()
            );
        }
    };
    filters.extensions = args
        .strings("extensions")?
        .iter()
        .flat_map(|text| text.split(','))
        .filter_map(normalize_extension)
        .collect();
    filters.older_than =
        args.text("older_than")?.map(parse_older_than).transpose()?;
    filters.names = args.strings("name")?;
    filters.category =
        args.text("category")?.map(parse_category).transpose()?;
    filters.reclaim = args.text("reclaim")?.map(parse_reclaim).transpose()?;
    Ok(query)
}

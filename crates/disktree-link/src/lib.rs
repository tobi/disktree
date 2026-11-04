//! The link between a running PwrDisk window and the agent that drives it.
//!
//! `pwrdisk mcp` scans on its own, so an agent needs no window. When one is
//! open, though, the agent's marks should appear in the mosaic and the
//! free-space meter, and its `show` and `review` should move the screen.
//! This crate is the wire between the two: one request per line of JSON
//! over a per-user socket or Windows named pipe, one line back.
//!
//! When a window is open it is the agent's source of truth: the agent lists
//! and searches the tree the person is looking at, and its marks live in the
//! window. The listing and search logic is [`disktree_core::query`], shared
//! with `pwrdisk mcp`'s own scans, so both answer alike.
//!
//! The endpoint is the trust boundary, the same one the shell has:
//! only this user can reach it. There is no token and no approval step, by
//! design. Removal intents are not authentication credentials: the app
//! requires an expiring, single-use preview matching the confirmed paths,
//! mode and policy. The existing guards and capacity limit apply to both
//! human review and confirmed MCP removal.

use std::path::{Path, PathBuf};

use disktree_core::classify::Category;
use disktree_core::query::{Filter, Kind};
use disktree_core::tree::Node;
use serde::{Deserialize, Serialize};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{Incoming, Link, Listener};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Incoming, Link, Listener};

#[cfg(not(any(unix, windows)))]
mod unsupported;
#[cfg(not(any(unix, windows)))]
pub use unsupported::{Incoming, Link, Listener};

#[cfg(all(test, any(unix, windows)))]
mod transport_tests;

/// Whether a window can be linked on this system at all.
pub const SUPPORTED: bool = cfg!(any(unix, windows));

/// Protocol version, bumped when a message changes shape. Both sides are
/// built from one tree, so a mismatch means an old window is still open.
pub const VERSION: u32 = 8;

/// Updating the GUI does not replace a process hosted by an MCP client.
pub const RECONNECT_MCP: &str = "Updating PwrDisk's GUI does not restart its MCP server. In your MCP client, restart/reconnect the PwrDisk MCP server using `pwrdisk mcp` from the updated installation. If the GUI is outdated, quit and reopen the updated PwrDisk app.";

pub fn protocol_mismatch(window: u32, server: u32) -> String {
    format!(
        "Incompatible PwrDisk link protocols (GUI {window}; MCP server {server}). {RECONNECT_MCP}"
    )
}

/// Environment variable naming the endpoint, for tests and a second copy
/// of PwrDisk that should not meet the first.
pub const SOCKET_ENV: &str = "PWRDISK_LINK";

/// Where the window listens: `$PWRDISK_LINK` if set, else a socket in the
/// user's runtime directory, or a Windows pipe named for the user's SID.
#[cfg(not(windows))]
pub fn socket_path() -> PathBuf {
    socket_path_in(
        std::env::var_os(SOCKET_ENV).map(PathBuf::from),
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        std::env::var_os("TMPDIR").map(PathBuf::from),
        std::env::var_os("USER").map(|user| user.to_string_lossy().into()),
    )
}

#[cfg(windows)]
pub fn socket_path() -> PathBuf {
    std::env::var_os(SOCKET_ENV).map_or_else(
        // Fail closed if Windows cannot identify this user. An empty path
        // is rejected by both ends rather than sharing a fallback name.
        || windows::default_path().unwrap_or_default(),
        PathBuf::from,
    )
}

/// [`socket_path`] over given environment values, so the choice is testable.
///
/// The runtime directory and `$TMPDIR` are already the user's own, so the
/// name needs no uid there; `/tmp` is shared, so the name carries the user.
pub fn socket_path_in(
    override_path: Option<PathBuf>,
    runtime_dir: Option<PathBuf>,
    tmp_dir: Option<PathBuf>,
    user: Option<String>,
) -> PathBuf {
    if let Some(path) = override_path {
        return path;
    }
    match runtime_dir.or(tmp_dir) {
        Some(dir) => dir.join("pwrdisk.sock"),
        None => PathBuf::from("/tmp").join(format!(
            "pwrdisk-{}.sock",
            user.unwrap_or_else(|| "user".into())
        )),
    }
}

/// One path the agent wants marked, with what it measured so the window
/// can show a size before its own scan of that root lands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkRequest {
    pub path: PathBuf,
    pub bytes: u64,
    pub is_dir: bool,
}

/// How a scan should measure, for [`Request::Scan`]. `None` keeps what the
/// window has.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanOptionsRequest {
    pub apparent_size: Option<bool>,
    pub include_hidden: Option<bool>,
    pub one_filesystem: Option<bool>,
    /// `Some(None)` lifts a depth limit; `Some(Some(n))` sets one.
    pub max_depth: Option<Option<usize>>,
}

/// A search, as the agent words it; see [`disktree_core::query::Filter`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindRequest {
    /// Where to search; the scanned root when absent.
    pub path: Option<PathBuf>,
    /// `any`, `file` or `dir`.
    pub kind: String,
    pub min_bytes: u64,
    /// A [`Category::name`].
    pub category: Option<String>,
    pub reclaimable: bool,
    pub name: Option<String>,
    pub before: Option<i64>,
    pub limit: usize,
}

impl FindRequest {
    /// The filter this asks for, or why it cannot be one.
    pub fn filter(&self) -> Result<Filter, String> {
        let kind = Kind::named(&self.kind)
            .ok_or_else(|| format!("unknown kind {}", self.kind))?;
        let category = match &self.category {
            Some(name) => Some(
                Category::named(name)
                    .ok_or_else(|| format!("unknown category {name}"))?,
            ),
            None => None,
        };
        Ok(Filter {
            kind,
            min_bytes: self.min_bytes,
            category,
            reclaimable: self.reclaimable,
            name: self
                .name
                .as_deref()
                .map(str::to_lowercase)
                .filter(|name| !name.is_empty()),
            before: self.before,
        })
    }
}

/// Removal modes are explicit; there is no fallback from Trash to deletion.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovalChoice {
    Trash,
    Permanent,
    OwnerCleanup,
}

impl RemovalChoice {
    pub const fn mode(self) -> disktree_core::removal::RemovalMode {
        match self {
            Self::Trash => disktree_core::removal::RemovalMode::Trash,
            Self::Permanent => disktree_core::removal::RemovalMode::Permanent,
            Self::OwnerCleanup => {
                disktree_core::removal::RemovalMode::OwnerCleanup
            }
        }
    }
    pub const fn confirmation(self) -> &'static str {
        match self {
            Self::Trash => "MOVE TO TRASH",
            Self::Permanent => "PERMANENTLY DELETE",
            Self::OwnerCleanup => "RUN OWNER CLEANUP",
        }
    }
    pub const fn warning(self) -> &'static str {
        match self {
            Self::Trash => {
                "Destructive: moves the listed paths to Trash or Recycle Bin. Recovery may be possible until emptied, but is not guaranteed. Space usually returns only when the user empties it."
            }
            Self::Permanent => {
                "Destructive: permanently deletes the listed paths. This cannot be undone through PwrDisk or Trash."
            }
            Self::OwnerCleanup => {
                "Destructive owner-managed cleanup: invokes only the previewed owner commands for their entire stated scopes, including unselected siblings. No Trash recovery and no raw deletion fallback. Stop the owning tools before approval. Listed allocation is not guaranteed freed space; APFS snapshots, clones and open files can retain blocks."
            }
        }
    }
}

/// A short-lived app preview of the exact effective targets and mode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalPreview {
    pub preview_id: String,
    pub root: PathBuf,
    pub mode: RemovalChoice,
    pub paths: Vec<PathBuf>,
    pub bytes: u64,
    pub estimate: SelectionEstimate,
    pub limit_percent: u8,
    pub volumes: Vec<disktree_core::removal_limit::Budget>,
    pub expires_in_seconds: u64,
    /// Actual owner command scopes. Empty for raw removal modes.
    pub cleanup: Vec<disktree_core::cleanup::Operation>,
}

/// Supplied only after the agent obtains explicit user confirmation of
/// the preview's exact paths, mode and consequences. Not a bearer credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmedRemoval {
    pub preview_id: String,
    pub mode: RemovalChoice,
    pub paths: Vec<PathBuf>,
    pub confirmation: String,
    /// Rescan once the removal finishes. Without it the window keeps its
    /// tree, with the removed paths shown as removed until a scan, which is
    /// what an agent removing several batches wants: a rescan can take
    /// minutes.
    #[serde(default)]
    pub rescan: bool,
}

/// What the agent asks of the window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Introduce the agent by its client's name; answered with [`State`].
    Hello { client: String, version: u32 },
    /// What the window shows now; answered with [`State`].
    State,
    /// Scan `root`, rescanning when it is the root already. Answered with
    /// [`State`] at once; the scan lands when `scanning` turns false.
    Scan {
        root: PathBuf,
        options: ScanOptionsRequest,
    },
    /// What is in a directory of the window's tree; answered with
    /// [`Listing`].
    Ls { path: PathBuf, limit: usize },
    /// The largest entries matching a description; answered with
    /// [`FoundList`].
    Find(FindRequest),
    /// Read-only hypothetical selection, with bounded retained clone peers.
    /// Does not change marks, navigation or a pending removal preview.
    Estimate {
        paths: Vec<PathBuf>,
        peer_scope: Option<PathBuf>,
        peer_limit: usize,
    },
    /// Show `path` in the mosaic. `root` is the agent's scan root: when the
    /// window shows something else, it rescans from there first.
    Show { root: PathBuf, path: PathBuf },
    /// Mark these paths, under the agent's scan `root`. The window measures
    /// each from its own tree when it can; `bytes` stands in until then.
    /// Already marked paths stay marked; answered with [`Response::Marked`].
    Mark {
        root: PathBuf,
        targets: Vec<MarkRequest>,
    },
    /// Unmark these paths, or everything when the list is empty; answered
    /// with [`State`].
    Unmark { paths: Vec<PathBuf> },
    /// Open the review screen over what is marked.
    Review,
    /// Prepare a single-use intent; does not mark, navigate or remove.
    PrepareRemoval { mode: RemovalChoice },
    /// A destructive write, only after explicit confirmation of the preview.
    Remove(ConfirmedRemoval),
}

/// One entry of the tree, as the agent reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub bytes: u64,
    pub files: u64,
    /// A [`Category::name`].
    pub category: String,
    /// Why its space can be had back, if it can.
    pub reclaim: Option<String>,
    /// Artifact identity and cleanup assessment are independent. Missing
    /// on older peers; neither detection nor suggestion approves removal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<disktree_core::artifact::Detection>,
    /// Newest write at or beneath it, Unix seconds; `0` when unknown.
    pub modified: i64,
    pub read_error: bool,
    /// APFS clone sharing beneath it; absent when no file there is a clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clones: Option<Clones>,
    /// Removed since the scan, itself or with a directory holding it. The
    /// window keeps the scanned tree until a rescan, so `bytes` is still
    /// what the scan measured. Only a window sets it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub removed: bool,
    /// Scanned bytes of what was removed beneath it since the scan, when it
    /// is not wholly removed; already inside `bytes`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub removed_bytes: u64,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
const fn is_zero(bytes: &u64) -> bool {
    *bytes == 0
}

/// How an entry's listed bytes split once APFS clones are accounted for.
/// `bytes` stays the allocated total; this only annotates it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clones {
    /// Listed bytes outside clone files, plus clone files' private data.
    pub private: u64,
    /// Clone blocks other clones may hold, inside this scan or not.
    pub shared: u64,
    /// Bytes listed more than once beneath it, clones of one another.
    pub repeated: u64,
    /// What removing this entry alone frees, before snapshots: a group is
    /// credited only when every clone APFS counts is beneath it.
    pub lower: u64,
    pub upper: u64,
    /// Some clone facts are missing, or too many groups to track.
    pub partial: bool,
}

impl Clones {
    /// `4.0 KiB private · 12 MiB shared with other APFS clones`, as the
    /// window and an agent's rows both read it; `None` without clone
    /// blocks. The refcount counts clones outside the scan too, so the
    /// wording never says the blocks are shared only within it.
    pub fn split(&self) -> Option<String> {
        use disktree_core::size::human_bytes;
        (self.shared > 0).then(|| {
            format!(
                "{} private \u{00b7} {} shared with other APFS clones",
                human_bytes(self.private),
                human_bytes(self.shared)
            )
        })
    }

    pub fn of(node: Node<'_>) -> Option<Self> {
        let sharing = node.sharing();
        // Only clone files carry these facts: off macOS, in an apparent
        // size scan or with clone metadata off there are none, and the
        // entry reads exactly as it did before clones were reported.
        if sharing.files == 0 {
            return None;
        }
        let shared = sharing.shared_bytes().min(node.bytes);
        Some(Self {
            private: node.bytes - shared,
            shared,
            repeated: sharing.duplicate_bytes,
            lower: sharing.reclaimable(node.bytes),
            upper: sharing.possible(node.bytes),
            partial: sharing.groups_truncated
                || sharing.unknown_files > 0
                || sharing.estimate.is_some_and(|estimate| estimate.partial),
        })
    }
}

impl Entry {
    pub fn at(node: Node<'_>, path: &Path) -> Self {
        let mut entry = Self::of(node);
        entry.artifact = disktree_core::artifact::assess(
            path,
            disktree_core::path_style::PathStyle::Native,
            node,
        );
        entry
    }

    pub fn of(node: Node<'_>) -> Self {
        Self {
            name: node.name().to_string(),
            is_dir: node.is_dir(),
            bytes: node.bytes,
            files: node.files,
            category: node.category.name().to_string(),
            reclaim: node.reclaim.map(|reclaim| reclaim.label().to_string()),
            artifact: None,
            modified: node.modified,
            read_error: node.read_error,
            clones: Clones::of(node),
            removed: false,
            removed_bytes: 0,
        }
    }
}

/// A directory and its largest children.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listing {
    pub path: PathBuf,
    pub entry: Entry,
    /// The scanned root's total, for the directory's share of it.
    pub root_bytes: u64,
    /// The scanned root, for naming it.
    pub root: PathBuf,
    /// Largest first, at most the limit asked for.
    pub children: Vec<Entry>,
    /// Children past the limit, and their bytes together.
    pub more: usize,
    pub more_bytes: u64,
}

impl Listing {
    /// List `node`, which sits at `path` in a tree rooted at `root` whose
    /// total is `root_bytes`.
    pub fn of(
        root: &Path,
        root_bytes: u64,
        path: &Path,
        node: Node<'_>,
        limit: usize,
    ) -> Self {
        let children: Vec<Entry> = node
            .children()
            .iter()
            .take(limit)
            .map(|child| Entry::at(child, &path.join(child.name())))
            .collect();
        let more = node.children().len().saturating_sub(limit);
        let more_bytes = node
            .children()
            .iter()
            .skip(limit)
            .map(|child| child.bytes)
            .sum();
        Self {
            path: path.to_path_buf(),
            entry: Entry::at(node, path),
            root_bytes,
            root: root.to_path_buf(),
            children,
            more,
            more_bytes,
        }
    }
}

/// One search match, with where it is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Found {
    pub path: PathBuf,
    pub entry: Entry,
}

/// What a search found; see [`disktree_core::query::Found`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundList {
    pub matches: Vec<Found>,
    pub count: usize,
    pub bytes: u64,
}

impl FoundList {
    pub fn of(found: &disktree_core::query::Found<'_>) -> Self {
        Self {
            matches: found
                .matches
                .iter()
                .map(|(path, node)| Found {
                    path: path.clone(),
                    entry: Entry::at(*node, path),
                })
                .collect(),
            count: found.count,
            bytes: found.bytes,
        }
    }
}

/// The tree a window has, in totals.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeSummary {
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
    /// Unix seconds when it landed.
    pub scanned_at: i64,
    pub elapsed_ms: u64,
    pub unreadable: u64,
    pub apparent_size: bool,
    pub include_hidden: bool,
}

/// One "worth a look" finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Insight {
    pub path: PathBuf,
    pub bytes: u64,
    pub what: String,
}

/// A mark the window would not take, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refused {
    pub path: PathBuf,
    pub reason: String,
}

/// A marked path as the window holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    pub path: PathBuf,
    pub bytes: u64,
    pub is_dir: bool,
}

/// Bounds for the whole accepted union, before snapshots and later changes.
/// Per-mark bounds cannot represent groups spanning several marks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionEstimate {
    pub paths: Vec<PathBuf>,
    pub lower: u64,
    /// Unknown when no physical measurement supports a bound (for example,
    /// apparent lengths can be smaller than allocated blocks).
    pub upper: Option<u64>,
    /// Only this lower bound belongs on the scanned volume's meter.
    pub on_volume: u64,
    /// A limited scan, unmeasured allocation or incomplete sharing metadata.
    pub partial: bool,
    /// Listed bytes of the selection in APFS clone blocks that other clones
    /// may hold; [`Sharing::shared_bytes`] summed. Zero off APFS.
    ///
    /// [`Sharing::shared_bytes`]: disktree_core::sharing::Sharing::shared_bytes
    #[serde(default)]
    pub shared: u64,
}

impl SelectionEstimate {
    pub fn of(plan: &disktree_core::removal::Plan) -> Self {
        Self {
            paths: plan.targets.iter().map(|t| t.path.clone()).collect(),
            lower: plan.reclaimable_bytes(),
            upper: Some(plan.possible_bytes()),
            on_volume: plan.reclaim(),
            partial: plan.estimate_partial(),
            shared: 0,
        }
    }

    /// Accepted targets never nest, so their shares add up.
    #[must_use]
    pub fn with_shared<'a>(
        mut self,
        nodes: impl IntoIterator<Item = Node<'a>>,
    ) -> Self {
        self.shared = nodes
            .into_iter()
            .map(|node| node.sharing().shared_bytes().min(node.bytes))
            .sum();
        self
    }

    pub fn unknown(plan: &disktree_core::removal::Plan) -> Self {
        Self {
            lower: 0,
            upper: None,
            on_volume: 0,
            partial: true,
            ..Self::of(plan)
        }
    }
}

/// The volume the window measures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Space {
    pub total: u64,
    pub free: u64,
    pub available: u64,
}

/// What the last removal did, kept until the next one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removal {
    /// `trash` or `permanent`.
    pub mode: String,
    pub removed: usize,
    pub failed: usize,
    /// Bytes of the targets removed, as marked.
    pub bytes: u64,
    /// Free space gained on the volume, measured before and after; `None`
    /// when the volume could not be read.
    pub freed: Option<u64>,
    /// Signed available-space delta for this run, including concurrent writes.
    #[serde(default)]
    pub observed: Option<i64>,
    /// Unix seconds when it finished.
    pub at: i64,
    pub cleanup: Vec<disktree_core::cleanup::Report>,
}

/// Outcome of the exact previewed selection, including native UI removal.
/// Kept after completion and rescans so an outstanding form can reconcile.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalOperation {
    pub preview_id: String,
    pub mode: RemovalChoice,
    pub paths: Vec<PathBuf>,
    pub finished: bool,
    pub removed: usize,
    pub failed: usize,
    pub pending: usize,
}

/// One removal since the last scan, three ways.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalCohort {
    /// `trash` or `permanent`.
    pub mode: String,
    pub removed: u64,
    pub failed: usize,
    /// Listed bytes of what was removed; a clone counts in every place.
    pub reported: u64,
    /// What the scan saw as only theirs; `None` when it could not say.
    pub private: Option<u64>,
    /// Free space after minus before, which other programs move too.
    pub observed: Option<i64>,
    /// Unix seconds.
    pub started: i64,
    pub finished: i64,
}

/// Every removal since the last scan, and their totals: `observed` runs
/// from the first one's start to the last one's finish.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalLedger {
    pub cohorts: Vec<RemovalCohort>,
    pub reported: u64,
    pub private: Option<u64>,
    pub observed: Option<i64>,
}

/// What the window shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
    /// GUI package version, independent of the MCP server and link protocol.
    #[serde(default)]
    pub app_version: String,
    /// The scanned root.
    pub root: PathBuf,
    /// The directory on screen.
    pub current: PathBuf,
    /// `explore`, `review`, `running` or `done`.
    pub screen: String,
    /// A scan is in progress; the tree may be absent or about to change.
    pub scanning: bool,
    /// A tree is on screen.
    pub scanned: bool,
    pub marks: Vec<Mark>,
    /// Absent until a complete local tree can supply the selected union.
    #[serde(default)]
    pub estimate: Option<SelectionEstimate>,
    pub space: Option<Space>,
    pub removal: Option<Removal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removal_operation: Option<RemovalOperation>,
    /// The removals since the scan on screen, which the tree already
    /// leaves out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removals: Option<RemovalLedger>,
    /// The tree on screen, when there is one.
    pub tree: Option<TreeSummary>,
    /// The biggest things that could plausibly go, as the panel lists them.
    pub insights: Vec<Insight>,
    /// The SSH host the window is showing, if not this computer. The
    /// agent's tools measure and mark this computer only, so a window on a
    /// host answers nothing but `hello` and `state`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

/// Why a window on an SSH host does not do what an agent asks.
pub fn remote_refusal(host: &str) -> String {
    format!(
        "The PwrDisk window is showing the SSH host {host}. The agent's \
         tools measure and mark this computer only, and PwrDisk does not \
         remove files over SSH. Ask the user to choose This computer in the \
         window to bring it along, or to close it; until then the agent \
         works without the window."
    )
}

/// The window's answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    State(State),
    /// The marks taken, and the ones not.
    Marked {
        state: State,
        refused: Vec<Refused>,
    },
    Listing(Listing),
    Found(FoundList),
    Estimate(disktree_core::planning::Report),
    RemovalPreview(RemovalPreview),
    Ok,
    Error {
        message: String,
    },
}

impl Response {
    pub fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
        }
    }
}

/// A line longer than this is not a request; it is a mistake or an attack
/// on memory, and the connection is dropped.
#[cfg(any(unix, windows))]
const MAX_LINE: usize = 8 * 1024 * 1024;

#[cfg(any(unix, windows))]
fn encode<T: Serialize>(value: &T) -> std::io::Result<String> {
    let mut line =
        serde_json::to_string(value).map_err(std::io::Error::other)?;
    line.push('\n');
    Ok(line)
}

#[cfg(any(unix, windows))]
fn decode<T: for<'de> Deserialize<'de>>(line: &str) -> std::io::Result<T> {
    serde_json::from_str(line.trim_end()).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(unix, windows))]
    #[test]
    fn requests_round_trip_through_json_lines() {
        let request = Request::Mark {
            root: PathBuf::from("/home/u"),
            targets: vec![MarkRequest {
                path: PathBuf::from("/home/u/.cache"),
                bytes: 42,
                is_dir: true,
            }],
        };
        let line = encode(&request).expect("encode");
        assert!(line.ends_with('\n'));
        assert!(line.contains("\"op\":\"mark\""));
        let back: Request = decode(&line).expect("decode");
        assert_eq!(back, request);
    }

    /// A directory of `copies` files, each a clone of one group that APFS
    /// says has `references` members.
    fn clones(
        copies: u64,
        references: u32,
        private: Option<u64>,
    ) -> disktree_core::tree::Tree {
        use disktree_core::sharing::{Allocation, CloneInfo};
        use disktree_core::tree::{Draft, Metric, NodeKind, Tree, aggregate};
        let mut root = Draft::directory("root");
        for index in 0..copies {
            let mut dir = Draft::directory(format!("copy-{index}"));
            let mut file = Draft::entry("data", NodeKind::File, 1000);
            file.allocation = Some(Box::new(Allocation {
                bytes: 1000,
                identity: None,
                links: 1,
                followed: false,
                clone: Some(CloneInfo {
                    device: 1,
                    id: 7,
                    references,
                    private_bytes: private,
                    data_bytes: 1000,
                }),
            }));
            dir.children.push(file);
            root.children.push(dir);
        }
        root.children.push(Draft::entry(
            "plain",
            disktree_core::tree::NodeKind::File,
            50,
        ));
        let mut root = Tree::from(root);
        aggregate(&mut root, Metric::Bytes);
        root
    }

    #[test]
    fn a_full_clone_is_all_shared_and_frees_nothing_alone() {
        let root = clones(3, 3, Some(0));
        let copy = Clones::of(root.root().children().at(0)).expect("a clone");
        assert_eq!(
            copy,
            Clones {
                private: 0,
                shared: 1000,
                repeated: 0,
                lower: 0,
                upper: 0,
                partial: false,
            }
        );
        // Every member is beneath the root, so the group is freed once.
        let all = Clones::of(root.root()).expect("clones beneath");
        assert_eq!((all.private, all.shared, all.repeated), (50, 3000, 2000));
        assert_eq!((all.lower, all.upper), (1050, 1050));
    }

    #[test]
    fn a_member_outside_the_scan_keeps_the_group_from_being_credited() {
        let root = clones(3, 4, Some(0));
        let all = Clones::of(root.root()).expect("clones beneath");
        assert_eq!((all.lower, all.upper), (50, 50));
        assert_eq!(all.repeated, 2000);
    }

    #[test]
    fn a_modified_clone_counts_its_private_bytes_only() {
        let root = clones(3, 3, Some(300));
        let copy = Clones::of(root.root().children().at(0)).expect("a clone");
        assert_eq!((copy.private, copy.shared), (300, 700));
        assert_eq!((copy.lower, copy.upper), (300, 1000));
        assert!(!copy.partial);
    }

    #[test]
    fn missing_clone_facts_are_partial() {
        let mut root = clones(2, 0, Some(0));
        assert!(
            Clones::of(root.root().children().at(0))
                .expect("a clone")
                .partial
        );
        root = clones(2, 2, None);
        let copy = Clones::of(root.root().children().at(0)).expect("a clone");
        assert!(copy.partial);
        // An unknown private size is never claimed as private.
        assert_eq!((copy.private, copy.shared, copy.lower), (0, 1000, 0));
    }

    #[test]
    fn entries_without_clones_carry_nothing_and_old_payloads_decode() {
        let root = clones(1, 2, Some(0));
        let plain = Entry::of(root.root().children().at(1));
        assert_eq!(plain.name, "plain");
        assert_eq!(plain.clones, None);
        let line = serde_json::to_string(&plain).expect("encode");
        assert!(!line.contains("clones"), "{line}");
        // A version 4 entry has no `clones` key at all.
        let old: Entry = serde_json::from_str(
            r#"{"name":"a","is_dir":true,"bytes":1,"files":1,
                "category":"other","reclaim":null,"modified":0,
                "read_error":false}"#,
        )
        .expect("decode");
        assert_eq!(old.clones, None);
        let clone = Entry::of(root.root().children().at(0));
        assert!(clone.clones.is_some());
        let back: Entry =
            serde_json::from_str(&serde_json::to_string(&clone).expect("enc"))
                .expect("dec");
        assert_eq!(back, clone);
        let estimate: SelectionEstimate = serde_json::from_str(
            r#"{"paths":[],"lower":0,"upper":0,"on_volume":0,
                "partial":false}"#,
        )
        .expect("decode");
        assert_eq!(estimate.shared, 0);
    }

    #[test]
    fn listings_keep_vm_detection_separate_from_cleanup_suggestions() {
        use disktree_core::artifact::{Artifact, Disposition};
        use disktree_core::classify::classify_at;
        use disktree_core::path_style::PathStyle;
        use disktree_core::tree::{Draft, NodeKind, Tree};
        let mut root = Draft::directory("project");
        let mut dependencies = Draft::directory("node_modules");
        dependencies.children.push(Draft::entry(
            "package.js",
            NodeKind::File,
            10,
        ));
        root.children =
            vec![dependencies, Draft::entry("guest.vmss", NodeKind::File, 20)];
        let mut tree = Tree::from(root);
        classify_at(&mut tree, Path::new("/project"), PathStyle::Posix);
        let listing = Listing::of(
            Path::new("/project"),
            30,
            Path::new("/project"),
            tree.root(),
            10,
        );
        let vm = listing
            .children
            .iter()
            .find(|e| e.name == "guest.vmss")
            .unwrap();
        assert_eq!(vm.reclaim, None);
        let d = vm.artifact.unwrap();
        assert_eq!(d.kind, Artifact::VmSuspend);
        assert_eq!(d.disposition, Disposition::Detected);
        let dependencies = listing
            .children
            .iter()
            .find(|e| e.name == "node_modules")
            .unwrap();
        assert_eq!(
            dependencies.artifact.unwrap().disposition,
            Disposition::Suggested
        );
        let encoded = serde_json::to_string(&listing).unwrap();
        assert!(encoded.contains("\"disposition\":\"detected\""));
        assert_eq!(serde_json::from_str::<Listing>(&encoded).unwrap(), listing);
    }

    #[test]
    fn removed_status_round_trips_and_is_absent_when_nothing_went() {
        let root = clones(1, 2, Some(0));
        let plain = Entry::of(root.root().children().at(1));
        assert!(!plain.removed && plain.removed_bytes == 0);
        let line = serde_json::to_string(&plain).expect("encode");
        assert!(!line.contains("removed"), "older peers see no new keys");
        let old: Entry = serde_json::from_str(&line).expect("decode");
        assert!(!old.removed && old.removed_bytes == 0);
        for entry in [
            Entry {
                removed: true,
                ..plain.clone()
            },
            Entry {
                removed_bytes: 4096,
                ..plain
            },
        ] {
            let back: Entry = serde_json::from_str(
                &serde_json::to_string(&entry).expect("encode"),
            )
            .expect("decode");
            assert_eq!(back, entry);
        }
    }

    #[test]
    fn the_socket_is_the_override_else_a_per_user_place() {
        let custom = PathBuf::from("/x/custom.sock");
        assert_eq!(
            socket_path_in(Some(custom.clone()), None, None, None),
            custom
        );
        assert_eq!(
            socket_path_in(None, Some("/run/user/1000".into()), None, None),
            PathBuf::from("/run/user/1000/pwrdisk.sock")
        );
        assert_eq!(
            socket_path_in(None, None, Some("/var/t".into()), None),
            PathBuf::from("/var/t/pwrdisk.sock")
        );
        assert_eq!(
            socket_path_in(None, None, None, Some("ann".into())),
            PathBuf::from("/tmp/pwrdisk-ann.sock")
        );
    }
}

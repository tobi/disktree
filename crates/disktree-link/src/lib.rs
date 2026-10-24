//! The link between a running disktree window and the agent that drives it.
//!
//! `disktree mcp` scans on its own, so an agent needs no window. When one is
//! open, though, the agent's marks should appear in the mosaic and the
//! free-space meter, and its `show` and `review` should move the screen.
//! This crate is the wire between the two: one request per line of JSON
//! over a per-user socket, one line back.
//!
//! When a window is open it is the agent's source of truth: the agent lists
//! and searches the tree the person is looking at, and its marks live in the
//! window. The listing and search logic is [`disktree_core::query`], shared
//! with `disktree mcp`'s own scans, so both answer alike.
//!
//! The socket is the trust boundary, and it is the same one the shell has:
//! only this user can reach it. There is no token and no approval step, by
//! design. The agent cannot remove anything over it; the review screen it
//! opens is where the person decides.

use std::path::{Path, PathBuf};

use disktree_core::classify::Category;
use disktree_core::query::{Filter, Kind};
use disktree_core::tree::Node;
use serde::{Deserialize, Serialize};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{Incoming, Link, Listener};

#[cfg(not(unix))]
mod unsupported;
#[cfg(not(unix))]
pub use unsupported::{Incoming, Link, Listener};

/// Protocol version, bumped when a message changes shape. Both sides are
/// built from one tree, so a mismatch means an old window is still open.
pub const VERSION: u32 = 2;

/// Environment variable naming the socket, for tests and for a second copy
/// of disktree that should not meet the first.
pub const SOCKET_ENV: &str = "DISKTREE_LINK";

/// Where the window listens: `$DISKTREE_LINK` if set, else a socket in the
/// user's runtime directory.
pub fn socket_path() -> PathBuf {
    socket_path_in(
        std::env::var_os(SOCKET_ENV).map(PathBuf::from),
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        std::env::var_os("TMPDIR").map(PathBuf::from),
        std::env::var_os("USER").map(|user| user.to_string_lossy().into()),
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
        Some(dir) => dir.join("disktree.sock"),
        None => PathBuf::from("/tmp").join(format!(
            "disktree-{}.sock",
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
    /// Newest write at or beneath it, Unix seconds; `0` when unknown.
    pub modified: i64,
    pub read_error: bool,
}

impl Entry {
    pub fn of(node: &Node) -> Self {
        Self {
            name: node.name.to_string(),
            is_dir: node.is_dir(),
            bytes: node.bytes,
            files: node.files,
            category: node.category.name().to_string(),
            reclaim: node.reclaim.map(|reclaim| reclaim.label().to_string()),
            modified: node.modified,
            read_error: node.read_error,
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
        node: &Node,
        limit: usize,
    ) -> Self {
        let children: Vec<Entry> =
            node.children.iter().take(limit).map(Entry::of).collect();
        let more = node.children.len().saturating_sub(limit);
        let more_bytes = node
            .children
            .iter()
            .skip(limit)
            .map(|child| child.bytes)
            .sum();
        Self {
            path: path.to_path_buf(),
            entry: Entry::of(node),
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
                    entry: Entry::of(node),
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
    /// Unix seconds when it finished.
    pub at: i64,
}

/// What the window shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub version: u32,
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
    pub space: Option<Space>,
    pub removal: Option<Removal>,
    /// The tree on screen, when there is one.
    pub tree: Option<TreeSummary>,
    /// The biggest things that could plausibly go, as the panel lists them.
    pub insights: Vec<Insight>,
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
#[cfg(unix)]
const MAX_LINE: usize = 8 * 1024 * 1024;

#[cfg(unix)]
fn encode<T: Serialize>(value: &T) -> std::io::Result<String> {
    let mut line =
        serde_json::to_string(value).map_err(std::io::Error::other)?;
    line.push('\n');
    Ok(line)
}

#[cfg(unix)]
fn decode<T: for<'de> Deserialize<'de>>(line: &str) -> std::io::Result<T> {
    serde_json::from_str(line.trim_end()).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
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

    #[test]
    fn the_socket_is_the_override_else_a_per_user_place() {
        let custom = PathBuf::from("/x/custom.sock");
        assert_eq!(
            socket_path_in(Some(custom.clone()), None, None, None),
            custom
        );
        assert_eq!(
            socket_path_in(None, Some("/run/user/1000".into()), None, None),
            PathBuf::from("/run/user/1000/disktree.sock")
        );
        assert_eq!(
            socket_path_in(None, None, Some("/var/t".into()), None),
            PathBuf::from("/var/t/disktree.sock")
        );
        assert_eq!(
            socket_path_in(None, None, None, Some("ann".into())),
            PathBuf::from("/tmp/disktree-ann.sock")
        );
    }
}

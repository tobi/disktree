//! disktree-web: a browser-based mirror of the disktree treemap.
//!
//! Scans a root with `disktree-core` once, keeps the tree in memory, and
//! serves it lazily by path: `/api/node?crumbs=1,4` returns one node plus
//! one level of children, never the whole subtree. Sending the entire tree
//! in one response does not scale — a home directory with a few million
//! files serializes to hundreds of megabytes of JSON, which locks up the
//! browser tab before it can paint anything.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Query, State};
use axum::response::Json;
use axum::routing::get;
use clap::Parser;
use disktree_core::classify::{Category, Reclaim};
use disktree_core::scan::{self, ScanOptions};
use disktree_core::tree::{Node, NodeKind};
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;

#[derive(Parser, Debug)]
#[command(about = "Web dashboard mirroring disktree, over HTTP")]
struct Args {
    /// Directory to scan. Defaults to $HOME.
    root: Option<PathBuf>,

    /// Bind address, so it can be reached over Tailscale etc.
    #[arg(long, default_value = "0.0.0.0:7878")]
    listen: String,

    /// Measure apparent size instead of disk usage.
    #[arg(long)]
    apparent_size: bool,
}

/// JSON-friendly, *shallow* mirror of one `disktree_core::tree::Node`.
///
/// Only this node's own children are included, and each child in turn
/// carries `has_children` instead of its own children — the client fetches
/// a level at a time. Kept separate from the core `Node` on purpose: the
/// core crate has no serde dependency (`AGENTS.md` treats it as UI-free).
#[derive(Serialize)]
struct JsonNode {
    name: String,
    kind: &'static str,
    bytes: u64,
    files: u64,
    dirs: u64,
    modified: i64,
    category: &'static str,
    reclaim: Option<&'static str>,
    read_error: bool,
    /// This node's own crumb path, so the client can ask for it again.
    crumbs: Vec<usize>,
    has_children: bool,
    children: Vec<Self>,
}

const fn kind_label(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Directory => "directory",
        NodeKind::File => "file",
        NodeKind::Symlink => "symlink",
        NodeKind::Other => "other",
    }
}

const fn reclaim_label(reclaim: Reclaim) -> &'static str {
    reclaim.label()
}

/// One level deep: `node`'s direct children are fully described, but each
/// child's own children are left empty (`has_children` says whether more
/// is there to fetch).
fn to_json_shallow(node: &Node, crumbs: &[usize]) -> JsonNode {
    let children = node
        .children
        .iter()
        .enumerate()
        .map(|(i, child)| {
            let mut child_crumbs = crumbs.to_vec();
            child_crumbs.push(i);
            JsonNode {
                name: child.name.to_string(),
                kind: kind_label(child.kind),
                bytes: child.bytes,
                files: child.files,
                dirs: child.dirs,
                modified: child.modified,
                category: Category::label(child.category),
                reclaim: child.reclaim.map(reclaim_label),
                read_error: child.read_error,
                crumbs: child_crumbs,
                has_children: !child.children.is_empty(),
                children: Vec::new(),
            }
        })
        .collect();

    JsonNode {
        name: node.name.to_string(),
        kind: kind_label(node.kind),
        bytes: node.bytes,
        files: node.files,
        dirs: node.dirs,
        modified: node.modified,
        category: Category::label(node.category),
        reclaim: node.reclaim.map(reclaim_label),
        read_error: node.read_error,
        crumbs: crumbs.to_vec(),
        has_children: !node.children.is_empty(),
        children,
    }
}

struct AppState {
    /// The one full scan, kept in memory. A home directory scan can take
    /// several seconds; every request walks this instead of re-scanning.
    tree: Node,
}

#[derive(Deserialize)]
struct NodeQuery {
    /// Comma-separated child indices from the root, e.g. "1,4,0". Empty or
    /// absent means the root itself.
    crumbs: Option<String>,
}

fn parse_crumbs(raw: Option<&str>) -> Vec<usize> {
    raw.map(|s| {
        s.split(',')
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse().ok())
            .collect()
    })
    .unwrap_or_default()
}

async fn node_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<NodeQuery>,
) -> Json<JsonNode> {
    let crumbs = parse_crumbs(query.crumbs.as_deref());
    let node = state.tree.resolve(&crumbs).unwrap_or(&state.tree);
    Json(to_json_shallow(node, &crumbs))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let root = args
        .root
        .or_else(dirs_home)
        .ok_or_else(|| anyhow::anyhow!("no root given and $HOME is unset"))?;

    let options = ScanOptions {
        apparent_size: args.apparent_size,
        ..ScanOptions::default()
    };

    println!("scanning {}…", root.display());
    let scan_started = std::time::Instant::now();
    let tree = scan::scan(&root, options)?;
    println!(
        "scanned {} files, {} dirs in {:.1}s",
        tree.files,
        tree.dirs,
        scan_started.elapsed().as_secs_f64()
    );

    let state = Arc::new(AppState { tree });

    let static_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("static");

    let app = Router::new()
        .route("/api/node", get(node_handler))
        .fallback_service(ServeDir::new(static_dir))
        .with_state(state);

    let addr: SocketAddr = args.listen.parse()?;
    println!("disktree-web listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

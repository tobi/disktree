//! Versioned JSON (`--json`) and newline-delimited JSON (`--ndjson` /
//! `--stream`) output formatters.

use std::io::{self, Write};

use disktree_scanner::{
    ActiveStorageMode, FallbackReason, Freshness, HostIdentity, Metric, NodeId,
    NodeKind, ScanSnapshot, ScanStats, SpaceInfo, SubtreeStatus, TreeArena,
    unix_now,
};
use serde::Serialize;

/// Schema version emitted in every `--json` / `--ndjson` snapshot document.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Serialize)]
pub struct JsonDocument {
    pub schema_version: u32,
    pub host: Option<HostIdentity>,
    pub root: String,
    pub metric: Metric,
    pub apparent_size: bool,
    pub include_hidden: bool,
    pub freshness: JsonFreshness,
    pub storage: JsonStorage,
    pub space: Option<SpaceInfo>,
    pub stats: ScanStats,
    pub tree: JsonNode,
}

#[derive(Serialize)]
pub struct JsonFreshness {
    pub status: &'static str,
    pub scanned_at: Option<u64>,
    pub age_seconds: Option<u64>,
    pub stale_dirs: Option<u32>,
}

#[derive(Serialize)]
pub struct JsonStorage {
    pub mode: ActiveStorageMode,
    pub fallback_reason: Option<FallbackReason>,
}

#[derive(Serialize)]
pub struct JsonNode {
    pub name: String,
    pub path: String,
    pub kind: NodeKind,
    pub bytes: u64,
    pub apparent_bytes: u64,
    pub files: u64,
    pub dirs: u32,
    pub modified: i64,
    pub read_error: bool,
    pub hidden: bool,
    pub status: SubtreeStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Self>,
}

/// Build a structured [`JsonDocument`] from a [`ScanSnapshot`].
#[allow(clippy::too_many_arguments, reason = "top-level output builder")]
pub fn build_document(
    snapshot: &ScanSnapshot,
    metric: Metric,
    apparent_size: bool,
    include_hidden: bool,
    max_depth: Option<usize>,
    top_n: Option<usize>,
    space: Option<SpaceInfo>,
) -> JsonDocument {
    let now = unix_now();
    let freshness = match &snapshot.freshness {
        Freshness::Fresh { newest_at, .. } => JsonFreshness {
            status: "fresh",
            scanned_at: Some(*newest_at),
            age_seconds: Some(now.saturating_sub(*newest_at)),
            stale_dirs: Some(0),
        },
        Freshness::Stale {
            newest_at,
            stale_dirs,
            ..
        } => JsonFreshness {
            status: "stale",
            scanned_at: Some(*newest_at),
            age_seconds: Some(now.saturating_sub(*newest_at)),
            stale_dirs: Some(*stale_dirs),
        },
        Freshness::Partial {
            newest_at,
            missing_dirs,
            ..
        } => JsonFreshness {
            status: "partial",
            scanned_at: *newest_at,
            age_seconds: newest_at.map(|ts| now.saturating_sub(ts)),
            stale_dirs: Some(*missing_dirs),
        },
        Freshness::Live { scanned_at } => JsonFreshness {
            status: "live",
            scanned_at: Some(*scanned_at),
            age_seconds: Some(now.saturating_sub(*scanned_at)),
            stale_dirs: Some(0),
        },
    };

    let tree = build_node(
        &snapshot.arena,
        snapshot.arena.root(),
        0,
        max_depth,
        top_n,
        include_hidden,
    );

    JsonDocument {
        schema_version: SCHEMA_VERSION,
        host: HostIdentity::current(),
        root: snapshot.arena.root_path().to_string_lossy().into_owned(),
        metric,
        apparent_size,
        include_hidden,
        freshness,
        storage: JsonStorage {
            mode: snapshot.storage_mode,
            fallback_reason: snapshot.fallback_reason.clone(),
        },
        space,
        stats: snapshot.stats.clone(),
        tree,
    }
}

fn build_node(
    arena: &TreeArena,
    id: NodeId,
    depth: usize,
    max_depth: Option<usize>,
    top_n: Option<usize>,
    include_hidden: bool,
) -> JsonNode {
    let node = arena.node(id);
    let mut children = Vec::new();
    if node.is_dir() && max_depth.is_none_or(|limit| depth < limit) {
        let iter = arena
            .children(id)
            .filter(|&kid| include_hidden || !arena.node(kid).hidden());
        let limited: Vec<NodeId> = match top_n {
            Some(n) => iter.take(n).collect(),
            None => iter.collect(),
        };
        children.reserve(limited.len());
        for kid in limited {
            children.push(build_node(
                arena,
                kid,
                depth + 1,
                max_depth,
                top_n,
                include_hidden,
            ));
        }
    }

    JsonNode {
        name: arena.name(id).to_owned(),
        path: arena.path_of(id).to_string_lossy().into_owned(),
        kind: node.kind(),
        bytes: node.effective_allocated_bytes(),
        apparent_bytes: node.effective_apparent_bytes(),
        files: node.files,
        dirs: node.dirs,
        modified: node.modified,
        read_error: node.read_error(),
        hidden: node.hidden(),
        status: node.status(),
        children,
    }
}

/// Write a pretty-printed JSON document.
pub fn write_pretty<W: Write>(
    out: &mut W,
    doc: &JsonDocument,
) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *out, doc).map_err(io::Error::other)?;
    writeln!(out)
}

/// Write a single compact JSON line (`--ndjson`).
pub fn write_ndjson_line<W: Write, T: Serialize>(
    out: &mut W,
    value: &T,
) -> io::Result<()> {
    serde_json::to_writer(&mut *out, value).map_err(io::Error::other)?;
    writeln!(out)
}

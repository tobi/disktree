//! `disktree-agent`: disktree without a window, for scripts and AI agents.
//!
//! Two front ends share one report: `disktree scan PATH --json` prints it,
//! and `disktree mcp` serves it over the Model Context Protocol on stdio.
//! Both walk the tree with [`disktree_core::scan`], the scanner the window
//! uses, and both are **read-only by construction**: nothing in this crate
//! deletes, moves, trashes, renames or writes a file. It never names
//! [`disktree_core::removal`] or [`disktree_core::export`], and
//! `tests/read_only.rs` fails the build if it ever starts to.
//!
//! The JSON is described in `docs/agent-json.md` and versioned by its
//! `schema_version` field.

pub mod cli;
pub mod guide;
pub mod mcp;
pub mod query;
pub mod report;

/// The `schema_version` every report carries. Raised only when a field is
/// removed or changes meaning; new fields are added without raising it.
pub const SCHEMA_VERSION: u32 = 1;

/// This build's version, as reports and the MCP handshake give it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

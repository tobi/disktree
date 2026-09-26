//! `disktree-core`: scanning, layout, space accounting and deletion, with no
//! UI dependency.
//!
//! The split exists so the parts that must be *correct* — size accounting,
//! hardlink de-duplication, aspect-ratio layout, and what may be deleted — can
//! be built and tested without GPUI, a display, or a GPU.

pub mod access;
pub mod classify;
pub mod export;
pub mod filter;
pub mod git;
pub mod insights;
pub mod removal;
pub mod scan;
pub mod size;
pub mod space;
pub mod tree;
pub mod treemap;
#[cfg(windows)]
mod windows;

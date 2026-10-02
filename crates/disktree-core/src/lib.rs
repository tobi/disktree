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
pub mod insights;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod mft;
pub mod removal;
pub mod scan;
pub mod scan_threads;
pub mod size;
pub mod space;
pub mod tree;
pub mod treemap;
#[cfg(windows)]
mod windows;

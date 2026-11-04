//! A digest of a scanned tree, to compare two builds of the scanner:
//! `cargo run --release -p disktree-core --example tree_digest -- PATH`
//!
//! Hashes every node's name, totals, category, reclaim reason, sharing
//! figures and allocation record depth first, in child order, then again
//! after re-ranking by file count, so two trees agree only if their shape,
//! figures and order all do.
//!
//! Which name of a hardlinked file is charged depends on which worker
//! reaches it first, so set `NODEDUP=1` to compare two builds exactly.
//! `DUMP=1` prints one line per node as well, to find where they differ.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

use disktree_core::scan::{ScanOptions, scan};
use disktree_core::tree::{Metric, Node, aggregate};

fn digest(node: Node<'_>, hasher: &mut DefaultHasher) {
    node.name().hash(hasher);
    (
        node.bytes,
        node.files,
        node.dirs,
        node.modified,
        node.read_error,
    )
        .hash(hasher);
    format!("{:?}{:?}", node.category, node.reclaim).hash(hasher);
    // Every sharing figure and the file's allocation record, private
    // sizes and estimate bounds included: APFS clone accounting must not
    // move any of them.
    let sharing = node.sharing();
    format!("{sharing:?}{:?}", node.allocation()).hash(hasher);
    if std::env::var_os("DUMP").is_some() {
        println!(
            "{} {} {} {} {} {} {:?} {:?} {} {} {}",
            node.name(),
            node.bytes,
            node.files,
            node.dirs,
            node.modified,
            node.read_error,
            node.category,
            node.reclaim,
            sharing.files,
            sharing.bytes,
            node.children().len()
        );
    }
    node.children().len().hash(hasher);
    for child in node.children() {
        digest(child, hasher);
    }
}

fn main() -> std::io::Result<()> {
    let root = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("."), PathBuf::from);
    let mut tree = scan(
        &root,
        ScanOptions {
            dedup_hardlinks: std::env::var_os("NODEDUP").is_none(),
            ..ScanOptions::default()
        },
    )?;
    let mut hasher = DefaultHasher::new();
    digest(tree.root(), &mut hasher);
    let scanned = hasher.finish();
    aggregate(&mut tree, Metric::Files);
    let mut hasher = DefaultHasher::new();
    digest(tree.root(), &mut hasher);
    println!("scanned={scanned:016x} by_files={:016x}", hasher.finish());
    Ok(())
}

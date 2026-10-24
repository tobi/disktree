//! Questions an agent asks of a scanned tree: what is in a directory, and
//! the largest things matching a description.
//!
//! The window answers these over its link and `disktree mcp` answers them
//! over its own scans, so the logic lives here, once.

use std::path::{Path, PathBuf};

use crate::classify::Category;
use crate::tree::{Node, NodeKind};

/// What kind of entry a [`Filter`] matches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kind {
    /// Files and directories; a matching directory hides its contents.
    #[default]
    Any,
    File,
    Dir,
}

impl Kind {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::File => "file",
            Self::Dir => "dir",
        }
    }

    pub fn named(name: &str) -> Option<Self> {
        match name {
            "any" => Some(Self::Any),
            "file" => Some(Self::File),
            "dir" => Some(Self::Dir),
            _ => None,
        }
    }
}

/// What a search keeps. Every field narrows; the default keeps everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    pub kind: Kind,
    /// Only entries of at least this many bytes.
    pub min_bytes: u64,
    pub category: Option<Category>,
    /// Only space that can be had back.
    pub reclaimable: bool,
    /// Only names containing this, compared lowercased.
    pub name: Option<String>,
    /// Only entries last written before this Unix time. For a directory
    /// that is the newest write anywhere beneath it.
    pub before: Option<i64>,
}

impl Filter {
    pub fn matches(&self, node: &Node) -> bool {
        let kind_ok = match self.kind {
            Kind::Any => true,
            Kind::File => !node.is_dir(),
            Kind::Dir => node.is_dir(),
        };
        kind_ok
            && node.bytes >= self.min_bytes
            && self
                .category
                .is_none_or(|category| node.category == category)
            && (!self.reclaimable || node.reclaim.is_some())
            && self
                .name
                .as_deref()
                .is_none_or(|name| node.name.to_lowercase().contains(name))
            && self.before.is_none_or(|before| {
                node.modified > 0 && node.modified < before
            })
    }

    /// Collect matches beneath `node`, topmost only: a matching directory
    /// is listed once, without what is inside it. Sizes shrink going down,
    /// so a node below the minimum ends the walk there.
    pub fn collect<'a>(
        &self,
        path: &Path,
        node: &'a Node,
        found: &mut Vec<(PathBuf, &'a Node)>,
    ) {
        if node.bytes < self.min_bytes || node.kind == NodeKind::Other {
            return;
        }
        if self.matches(node) {
            found.push((path.to_path_buf(), node));
            return;
        }
        for child in &node.children {
            self.collect(&path.join(&*child.name), child, found);
        }
    }
}

/// What a search found.
#[derive(Clone, Debug, Default)]
pub struct Found<'a> {
    /// The largest matches, largest first, at most the limit asked for.
    pub matches: Vec<(PathBuf, &'a Node)>,
    /// How many matched in all.
    pub count: usize,
    /// Bytes of every match, not only the ones listed.
    pub bytes: u64,
}

/// The largest `limit` entries beneath `root` (at `root_path`) that pass
/// `filter`. The root itself is never a match.
pub fn find<'a>(
    root_path: &Path,
    root: &'a Node,
    filter: &Filter,
    limit: usize,
) -> Found<'a> {
    let mut matches = Vec::new();
    for child in &root.children {
        filter.collect(&root_path.join(&*child.name), child, &mut matches);
    }
    matches.sort_by_key(|(_, node)| std::cmp::Reverse(node.bytes));
    let count = matches.len();
    let bytes = matches.iter().map(|(_, node)| node.bytes).sum();
    matches.truncate(limit);
    Found {
        matches,
        count,
        bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::Reclaim;
    use crate::tree::{Metric, aggregate};

    fn tree() -> Node {
        let mut root = Node::directory("r");
        let mut cache = Node::directory(".cache");
        cache.reclaim = Some(Reclaim::Regenerable);
        cache.category = Category::Cache;
        let mut blob = Node::entry("blob.bin", NodeKind::File, 400);
        blob.modified = 1_000;
        cache.children.push(blob);
        let mut src = Node::directory("src");
        let mut target = Node::directory("target");
        target.reclaim = Some(Reclaim::BuildOutput);
        target
            .children
            .push(Node::entry("app", NodeKind::File, 300));
        src.children.push(target);
        let mut main = Node::entry("main.rs", NodeKind::File, 20);
        main.modified = 5_000;
        src.children.push(main);
        root.children.push(cache);
        root.children.push(src);
        root.children
            .push(Node::entry("Movie.iso", NodeKind::File, 250));
        aggregate(&mut root, Metric::Bytes);
        root
    }

    #[test]
    fn reclaimable_matches_are_topmost_and_largest_first() {
        let root = tree();
        let root_path = Path::new("/r");
        let found = find(
            root_path,
            &root,
            &Filter {
                reclaimable: true,
                ..Filter::default()
            },
            10,
        );
        let paths: Vec<PathBuf> =
            found.matches.iter().map(|(path, _)| path.clone()).collect();
        assert_eq!(
            paths,
            [
                root_path.join(".cache"),
                root_path.join("src").join("target")
            ]
        );
        assert_eq!(found.count, 2);
        assert_eq!(found.bytes, 700);
    }

    #[test]
    fn files_over_a_size_and_names_and_ages_filter() {
        let root = tree();
        let big = find(
            Path::new("/r"),
            &root,
            &Filter {
                kind: Kind::File,
                min_bytes: 260,
                ..Filter::default()
            },
            10,
        );
        assert_eq!(big.matches.len(), 2);
        assert_eq!(&*big.matches[0].1.name, "blob.bin");
        assert_eq!(&*big.matches[1].1.name, "app");

        let named = find(
            Path::new("/r"),
            &root,
            &Filter {
                name: Some("iso".into()),
                ..Filter::default()
            },
            1,
        );
        assert_eq!(named.count, 1);
        assert_eq!(&*named.matches[0].1.name, "Movie.iso");

        // Older than 2_000: blob.bin (1_000), not main.rs (5_000), and not
        // the unknown-age iso.
        let old = find(
            Path::new("/r"),
            &root,
            &Filter {
                kind: Kind::File,
                before: Some(2_000),
                ..Filter::default()
            },
            10,
        );
        assert_eq!(old.count, 1);
        assert_eq!(&*old.matches[0].1.name, "blob.bin");

        // The limit trims the list but not the totals.
        let one = find(Path::new("/r"), &root, &Filter::default(), 1);
        assert_eq!(one.matches.len(), 1);
        assert_eq!(one.count, 3);
    }
}

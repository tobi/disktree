//! Typeahead filtering: which parts of a tree match a name.
//!
//! A node whose name contains the needle (ignoring ASCII case) matches, and
//! is kept whole: everything under a matching directory goes with it. Its
//! ancestors are kept only partly, and sized by what matched beneath them,
//! so a filtered treemap shows exactly the matches, at their true relative
//! sizes, in the places they live.

use rustc_hash::FxHashMap;

use crate::tree::{Metric, Node};

/// How a node takes part in a filtered view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    /// It matches: drawn as usual, with everything beneath it.
    Whole,
    /// It holds matches: drawn with only those, at their size.
    Partial { bytes: u64, files: u64 },
}

/// The outcome of filtering a subtree by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Matches {
    /// The needle, lowercased.
    pub needle: String,
    /// Absolute crumbs of the subtree that was searched.
    pub base: Vec<usize>,
    /// Keyed by absolute crumbs. Only matches and their ancestors appear;
    /// a match's own descendants are implied.
    pub keep: FxHashMap<Vec<usize>, Keep>,
    /// Topmost matches: a match inside a match is not counted again.
    pub count: usize,
    pub bytes: u64,
    pub files: u64,
}

impl Matches {
    /// How the node at `crumbs` takes part: `None` when it is filtered out.
    /// Anything outside the searched subtree, and anything beneath a match,
    /// is kept whole.
    pub fn keep(&self, crumbs: &[usize]) -> Option<Keep> {
        if !crumbs.starts_with(&self.base) {
            return Some(Keep::Whole);
        }
        for length in self.base.len()..=crumbs.len() {
            match self.keep.get(&crumbs[..length]) {
                Some(Keep::Whole) => return Some(Keep::Whole),
                Some(partial) if length == crumbs.len() => {
                    return Some(*partial);
                }
                // Only the base may be absent: it holds the matches but is
                // not recorded, being where the search started.
                None if length > self.base.len() => return None,
                _ => {}
            }
        }
        // The base itself holds the matches.
        Some(Keep::Partial {
            bytes: self.bytes,
            files: self.files,
        })
    }

    /// The value a kept node is laid out by.
    pub fn value(keep: Keep, node: Node<'_>, metric: Metric) -> u64 {
        match (keep, metric) {
            (Keep::Whole, _) => node.value(metric),
            (Keep::Partial { bytes, .. }, Metric::Bytes) => bytes,
            (Keep::Partial { files, .. }, Metric::Files) => files,
        }
    }
}

/// Search `node`, found at absolute `base`, for names containing `needle`.
/// `None` for an empty needle: nothing is filtered.
pub fn filter(node: Node<'_>, base: &[usize], needle: &str) -> Option<Matches> {
    let needle = needle.trim().to_ascii_lowercase();
    if needle.is_empty() {
        return None;
    }
    let mut matches = Matches {
        needle,
        base: base.to_vec(),
        ..Matches::default()
    };
    let mut crumbs = base.to_vec();
    let (bytes, files) = visit(node, &mut crumbs, &mut matches);
    matches.bytes = bytes;
    matches.files = files;
    Some(matches)
}

/// Returns the bytes and files that matched at or beneath `node`'s
/// children, recording what to keep.
fn visit(
    node: Node<'_>,
    crumbs: &mut Vec<usize>,
    matches: &mut Matches,
) -> (u64, u64) {
    let mut total = (0, 0);
    for (index, child) in node.children().enumerate() {
        crumbs.push(index);
        if contains_ignoring_case(child.name(), &matches.needle) {
            matches.keep.insert(crumbs.clone(), Keep::Whole);
            matches.count += 1;
            total.0 += child.bytes();
            total.1 += child.files();
        } else if child.has_children() {
            let (bytes, files) = visit(child, crumbs, matches);
            if bytes > 0 || files > 0 {
                matches
                    .keep
                    .insert(crumbs.clone(), Keep::Partial { bytes, files });
                total.0 += bytes;
                total.1 += files;
            }
        }
        crumbs.pop();
    }
    total
}

/// Substring search ignoring ASCII case, without allocating: a filter runs
/// over every name in view on every keystroke.
fn contains_ignoring_case(haystack: &str, lower_needle: &str) -> bool {
    let (hay, needle) = (haystack.as_bytes(), lower_needle.as_bytes());
    if needle.is_empty() {
        return true;
    }
    if needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(left, right)| left.to_ascii_lowercase() == *right)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Draft, NodeKind, Tree};

    fn file(name: &str, bytes: u64) -> Draft {
        Draft::entry(name, NodeKind::File, bytes)
    }

    fn dir(name: &str, children: Vec<Draft>) -> Draft {
        let mut node = Draft::directory(name);
        node.children = children;
        node
    }

    fn tree() -> Tree {
        let root = dir(
            "root",
            vec![
                dir(
                    "src",
                    vec![
                        dir("App", vec![file("main.rs", 10)]),
                        file("notes", 5),
                    ],
                ),
                dir(
                    "apps",
                    vec![file("x", 100), dir("apple", vec![file("y", 7)])],
                ),
                file("readme", 1),
            ],
        );
        Tree::from_draft(root, Metric::Bytes)
    }

    fn crumbs_of(tree: &Tree, names: &[&str]) -> Vec<usize> {
        let mut node = tree.root();
        names
            .iter()
            .map(|name| {
                let index = node
                    .children()
                    .position(|child| child.name() == *name)
                    .expect("present");
                node = node.child(index).expect("present");
                index
            })
            .collect()
    }

    #[test]
    fn matches_are_kept_whole_and_their_ancestors_by_what_matched() {
        let root = tree();
        let found = filter(root.root(), &[], "APP").expect("a needle");
        assert_eq!(found.count, 2, "src/App and apps; apple is inside apps");
        assert_eq!(found.bytes, 10 + 107);
        assert_eq!(found.keep(&crumbs_of(&root, &["apps"])), Some(Keep::Whole));
        assert_eq!(
            found.keep(&crumbs_of(&root, &["apps", "x"])),
            Some(Keep::Whole),
            "inside a match"
        );
        assert_eq!(
            found.keep(&crumbs_of(&root, &["src"])),
            Some(Keep::Partial {
                bytes: 10,
                files: 1
            })
        );
        assert_eq!(found.keep(&crumbs_of(&root, &["src", "notes"])), None);
        assert_eq!(found.keep(&crumbs_of(&root, &["readme"])), None);
    }

    #[test]
    fn a_search_below_the_root_leaves_the_rest_alone() {
        let root = tree();
        let src = crumbs_of(&root, &["src"]);
        let found = filter(root.resolve(&src).expect("src"), &src, "main")
            .expect("needle");
        assert_eq!(found.keep(&crumbs_of(&root, &["apps"])), Some(Keep::Whole));
        assert_eq!(found.keep(&crumbs_of(&root, &["src", "notes"])), None);
        assert_eq!(
            found.keep(&src),
            Some(Keep::Partial {
                bytes: 10,
                files: 1
            })
        );
    }

    #[test]
    fn an_empty_needle_filters_nothing() {
        assert!(filter(tree().root(), &[], "  ").is_none());
        let found = filter(tree().root(), &[], "zzz").expect("needle");
        assert_eq!(found.count, 0);
        assert!(found.keep.is_empty());
    }

    #[test]
    fn case_is_ignored_without_allocating() {
        assert!(contains_ignoring_case("Cargo.TOML", "toml"));
        assert!(!contains_ignoring_case("ab", "abc"));
        assert!(contains_ignoring_case("x", ""));
    }
}

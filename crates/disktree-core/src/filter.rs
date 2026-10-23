//! Typeahead filtering: which parts of a tree match a name, a kind or a
//! size.
//!
//! A node whose name contains the needle (ignoring ASCII case) matches, and
//! is kept whole: everything under a matching directory goes with it. Its
//! ancestors are kept only partly, and sized by what matched beneath them,
//! so a filtered treemap shows exactly the matches, at their true relative
//! sizes, in the places they live.
//!
//! Some words in the needle are not part of the name: `kind:cache` asks
//! for a [`Category`], and may be given more than once for any of several;
//! `>1g` asks for files of at least that size and `<4g` of at most. See
//! [`Query`].

use rustc_hash::FxHashMap;

use crate::classify::Category;
use crate::tree::{Metric, Node};

/// What a filter asks of an entry; every part that is set must hold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// Text the name must contain, lowercased. Empty asks nothing.
    pub name: String,
    /// The kinds of data it may be: any one of them. Empty asks nothing.
    pub kinds: Vec<Category>,
    /// The least a file may hold. A size is asked of files only: a
    /// directory is as large as everything in it, so every directory above
    /// a large file would match and the small files beside it come along.
    pub min_bytes: Option<u64>,
    /// The most a file may hold.
    pub max_bytes: Option<u64>,
}

/// The word that asks for a kind: `kind:cache`.
pub const KIND_PREFIX: &str = "kind:";

/// The word that asks for a least size: `>500m`.
pub const LEAST_PREFIX: char = '>';

/// The word that asks for a most size: `<4g`.
pub const MOST_PREFIX: char = '<';

impl Query {
    /// Read a needle. A word that looks like a kind or a size but names
    /// none is left in the name, so a file really called `kind:x` can
    /// still be found.
    pub fn parse(needle: &str) -> Self {
        let mut query = Self::default();
        let mut words: Vec<String> = Vec::new();
        for word in needle.split_whitespace() {
            let word = word.to_ascii_lowercase();
            let kind = word.strip_prefix(KIND_PREFIX).and_then(kind_named);
            let least = word.strip_prefix(LEAST_PREFIX).and_then(parse_size);
            let most = word.strip_prefix(MOST_PREFIX).and_then(parse_size);
            if let Some(kind) = kind {
                if !query.kinds.contains(&kind) {
                    query.kinds.push(kind);
                }
            } else if least.is_some() {
                query.min_bytes = least;
            } else if most.is_some() {
                query.max_bytes = most;
            } else {
                words.push(word);
            }
        }
        query.name = words.join(" ");
        query
    }

    pub const fn is_empty(&self) -> bool {
        self.name.is_empty()
            && self.kinds.is_empty()
            && self.min_bytes.is_none()
            && self.max_bytes.is_none()
    }

    fn matches(&self, node: &Node) -> bool {
        let sized = self.min_bytes.is_some() || self.max_bytes.is_some();
        (self.name.is_empty() || contains_ignoring_case(&node.name, &self.name))
            && (self.kinds.is_empty() || self.kinds.contains(&node.category))
            && (!sized
                || (!node.is_dir()
                    && self.min_bytes.is_none_or(|least| node.bytes >= least)
                    && self.max_bytes.is_none_or(|most| node.bytes <= most)))
    }
}

/// The word a kind is asked for by, after [`KIND_PREFIX`].
pub const fn kind_key(category: Category) -> &'static str {
    match category {
        Category::Code => "code",
        Category::AgentScratch => "agent",
        Category::Toolchain => "toolchain",
        Category::Synced => "synced",
        Category::Git => "git",
        Category::Media => "media",
        Category::Documents => "documents",
        Category::Cache => "cache",
        Category::Other => "other",
    }
}

fn kind_named(key: &str) -> Option<Category> {
    Category::LEGEND
        .into_iter()
        .chain([Category::Other])
        .find(|category| kind_key(*category) == key)
}

/// `500m`, `1.5g`, `2gib`, `4096`: binary units, as sizes are shown.
fn parse_size(text: &str) -> Option<u64> {
    let digits = text
        .find(|char: char| !(char.is_ascii_digit() || char == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(digits);
    let number: f64 = number.parse().ok()?;
    let shift = match unit.trim_end_matches("ib").trim_end_matches('b') {
        "" => 0,
        "k" => 10,
        "m" => 20,
        "g" => 30,
        "t" => 40,
        _ => return None,
    };
    let bytes = number * f64::from(1_u32 << 10).powi(shift / 10);
    // Not a size: negative cannot be typed, but `inf` and overflow can.
    (bytes.is_finite() && bytes < 1e18).then_some(bytes as u64)
}

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
    /// What the needle asks for.
    pub query: Query,
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
    pub const fn value(keep: Keep, node: &Node, metric: Metric) -> u64 {
        match (keep, metric) {
            (Keep::Whole, _) => node.value(metric),
            (Keep::Partial { bytes, .. }, Metric::Bytes) => bytes,
            (Keep::Partial { files, .. }, Metric::Files) => files,
        }
    }
}

/// Search `node`, found at absolute `base`, for names containing `needle`.
/// `None` for an empty needle: nothing is filtered.
pub fn filter(node: &Node, base: &[usize], needle: &str) -> Option<Matches> {
    let needle = needle.trim().to_ascii_lowercase();
    let query = Query::parse(&needle);
    if query.is_empty() {
        return None;
    }
    let mut matches = Matches {
        needle,
        query,
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
    node: &Node,
    crumbs: &mut Vec<usize>,
    matches: &mut Matches,
) -> (u64, u64) {
    let mut total = (0, 0);
    for (index, child) in node.children.iter().enumerate() {
        crumbs.push(index);
        if matches.query.matches(child) {
            matches.keep.insert(crumbs.clone(), Keep::Whole);
            matches.count += 1;
            total.0 += child.bytes;
            total.1 += child.files;
        } else if !child.children.is_empty() {
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
    use crate::tree::{NodeKind, aggregate};

    fn file(name: &str, bytes: u64) -> Node {
        Node::entry(name, NodeKind::File, bytes)
    }

    fn dir(name: &str, children: Vec<Node>) -> Node {
        let mut node = Node::directory(name);
        node.children = children;
        node
    }

    #[test]
    fn a_needle_can_ask_for_a_kind_and_a_size_beside_a_name() {
        let query = Query::parse("  Blob kind:cache >1.5g kind:media <4g ");
        assert_eq!(query.name, "blob");
        assert_eq!(query.kinds, [Category::Cache, Category::Media]);
        assert_eq!(query.min_bytes, Some(3 << 29));
        assert_eq!(query.max_bytes, Some(4 << 30));
        assert_eq!(Query::parse(">500m").min_bytes, Some(500 << 20));
        assert_eq!(Query::parse(">2gib").min_bytes, Some(2 << 30));
        // Words that only look the part stay in the name.
        let odd = Query::parse("kind:plaid >big");
        assert_eq!(odd.name, "kind:plaid >big");
        assert!(odd.kinds.is_empty() && odd.min_bytes.is_none());
        for category in Category::LEGEND {
            let word = format!("{KIND_PREFIX}{}", kind_key(category));
            assert_eq!(Query::parse(&word).kinds, [category]);
        }
    }

    #[test]
    fn a_size_is_asked_of_files_and_a_kind_of_whatever_is_topmost() {
        let mut cache = dir("store", vec![file("a", 40), file("b", 2)]);
        cache.category = Category::Cache;
        for child in &mut cache.children {
            child.category = Category::Cache;
        }
        let mut root =
            dir("root", vec![cache, file("big", 50), file("small", 3)]);
        aggregate(&mut root, Metric::Bytes);

        // Only the files that large, wherever they are; the directory
        // holding one is kept for it, not whole.
        let large = filter(&root, &[], ">30").expect("a size");
        assert_eq!(large.count, 2);
        assert_eq!(large.bytes, 90);
        let store = root
            .children
            .iter()
            .position(|child| child.name.as_ref() == "store")
            .expect("store");
        assert_eq!(
            large.keep(&[store]),
            Some(Keep::Partial {
                bytes: 40,
                files: 1
            })
        );

        // A kind takes the directory of that kind whole, once.
        let kind = filter(&root, &[], "kind:cache").expect("a kind");
        assert_eq!(kind.count, 1);
        assert_eq!(kind.bytes, 42);
        assert_eq!(kind.keep(&[store]), Some(Keep::Whole));
        // A most as well as a least: only what lies between.
        let band = filter(&root, &[], ">30 <45").expect("a band");
        assert_eq!((band.count, band.bytes), (1, 40));
        // Any of several kinds.
        let either =
            filter(&root, &[], "kind:media kind:cache").expect("kinds");
        assert_eq!((either.count, either.bytes), (1, 42));
        // Both together: the large file of that kind alone.
        let both = filter(&root, &[], "kind:cache >30").expect("both");
        assert_eq!((both.count, both.bytes), (1, 40));
    }

    fn tree() -> Node {
        let mut root = dir(
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
        aggregate(&mut root, Metric::Bytes);
        root
    }

    fn crumbs_of(root: &Node, names: &[&str]) -> Vec<usize> {
        let mut node = root;
        names
            .iter()
            .map(|name| {
                let index = node
                    .children
                    .iter()
                    .position(|child| &*child.name == *name)
                    .expect("present");
                node = &node.children[index];
                index
            })
            .collect()
    }

    #[test]
    fn matches_are_kept_whole_and_their_ancestors_by_what_matched() {
        let root = tree();
        let found = filter(&root, &[], "APP").expect("a needle");
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
        assert!(filter(&tree(), &[], "  ").is_none());
        let found = filter(&tree(), &[], "zzz").expect("needle");
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

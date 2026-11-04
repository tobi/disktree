//! Filtering: which parts of a tree match a name, or a [`Rule`] on what
//! the files are.
//!
//! A node whose name contains the needle (ignoring ASCII case) matches, and
//! is kept whole: everything under a matching directory goes with it. Its
//! ancestors are kept only partly, and sized by what matched beneath them,
//! so a filtered treemap shows exactly the matches, at their true relative
//! sizes, in the places they live.
//!
//! A rule is judged per file, from its own kind, age and reclaimability,
//! and summed up the tree the same way: a directory is whole when every
//! file beneath it passes, partial when some do, and gone when none do.

use rustc_hash::FxHashMap;

use crate::classify::Category;
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
    /// Topmost matches: a match inside a match is not counted again. Only
    /// names are counted; a filter by [`Rule`] alone counts nothing.
    pub count: usize,
    pub bytes: u64,
    pub files: u64,
    /// What a file must be to be kept, besides matching the needle.
    pub rule: Option<Rule>,
}

/// What a filtered view keeps of each file, from its own facts: its kind,
/// whether its space is reclaimable, and how long ago it was written. The
/// legend sets it, one chip per kind and age.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rule {
    /// Kinds to keep, one bit per [`Category`] (see [`Self::bit`]); none
    /// set keeps every kind.
    pub only: u16,
    /// Kinds to leave out, whatever `only` says.
    pub hidden: u16,
    /// `Some(true)` keeps only reclaimable space, `Some(false)` leaves it
    /// out. Reclaimable is a property, not a kind, so it narrows `only`
    /// rather than adding to it.
    pub reclaimable: Option<bool>,
    /// Age buckets to keep, one bit per entry of `age_limits`; none set
    /// keeps every age.
    pub ages_only: u8,
    /// Age buckets to leave out.
    pub ages_hidden: u8,
    /// Each bucket's upper bound in days, ascending; a file belongs to the
    /// first one its age is within, or the last.
    pub age_limits: Vec<i64>,
    /// The moment ages are measured from, in Unix seconds.
    pub now: i64,
}

impl Rule {
    /// The bit for `category` in [`Self::only`] and [`Self::hidden`].
    pub const fn bit(category: Category) -> u16 {
        1 << category as u16
    }

    /// Whether the rule keeps everything.
    pub const fn is_empty(&self) -> bool {
        self.only == 0
            && self.hidden == 0
            && self.reclaimable.is_none()
            && self.ages_only == 0
            && self.ages_hidden == 0
    }

    /// Whether the rule keeps `node`, judged by its own facts. Meant for
    /// files: a directory's facts are its own or inherited, not a summary
    /// of what it holds.
    pub fn passes(&self, node: Node<'_>) -> bool {
        let kind = Self::bit(node.category);
        if self.hidden & kind != 0 || (self.only != 0 && self.only & kind == 0)
        {
            return false;
        }
        if self
            .reclaimable
            .is_some_and(|wanted| node.reclaim.is_some() != wanted)
        {
            return false;
        }
        if self.ages_only == 0 && self.ages_hidden == 0 {
            return true;
        }
        // An unknown write time is in no bucket: kept unless the rule
        // keeps only some ages.
        if node.modified <= 0 {
            return self.ages_only == 0;
        }
        let bit = 1 << self.age_bucket(node.modified);
        self.ages_hidden & bit == 0
            && (self.ages_only == 0 || self.ages_only & bit != 0)
    }

    fn age_bucket(&self, modified: i64) -> usize {
        age_bucket(self.now, &self.age_limits, modified)
    }
}

/// Which age chip a write time falls in: the first limit, in days, it is
/// within, or the last chip when it is older than them all.
fn age_bucket(now: i64, limits: &[i64], modified: i64) -> usize {
    let days = (now - modified) / 86_400;
    limits
        .iter()
        .position(|limit| days <= *limit)
        .unwrap_or_else(|| limits.len().saturating_sub(1))
}

/// What each legend chip would keep of a whole tree.
///
/// In the tree's own metric: the bytes a chip set to only would show.
/// Judged per file by the facts [`Rule::passes`] reads, so a chip's total
/// is the "Showing" figure a click on it gives.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shares {
    /// By kind, indexed by `Category as usize` (a kind is a bit of a
    /// `u16`, so there are at most sixteen).
    pub kinds: [u64; 16],
    /// Files with a cleanup suggestion.
    pub suggested: u64,
    /// By age chip, one per limit given; a file with no write time is in
    /// none.
    pub ages: Vec<u64>,
}

impl Shares {
    pub const fn kind(&self, category: Category) -> u64 {
        self.kinds[category as usize]
    }
}

/// Sum every file beneath `root` into [`Shares`], with ages measured from
/// `now` against `age_limits` in days, as a [`Rule`] measures them.
pub fn shares(root: Node<'_>, now: i64, age_limits: &[i64]) -> Shares {
    let mut shares = Shares {
        ages: vec![0; age_limits.len()],
        ..Shares::default()
    };
    // A stack, not recursion: a scan can be deeper than a thread's stack
    // is comfortable with. The root's children, as a filter starts there:
    // a scan of one file holds nothing to filter.
    let mut stack: Vec<Node<'_>> = root.children().iter().collect();
    while let Some(node) = stack.pop() {
        let children = node.children();
        if !children.is_empty() {
            stack.extend(children.iter());
            continue;
        }
        shares.kinds[node.category as usize] += node.bytes;
        if node.reclaim.is_some() {
            shares.suggested += node.bytes;
        }
        if node.modified > 0 && !age_limits.is_empty() {
            shares.ages[age_bucket(now, age_limits, node.modified)] +=
                node.bytes;
        }
    }
    shares
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

    /// [`Self::keep`], for `node` found at `crumbs`. A rule's files are
    /// judged as they are asked about, not recorded: a partly kept
    /// directory can hold millions of them.
    pub fn keep_at(&self, crumbs: &[usize], node: Node<'_>) -> Option<Keep> {
        let keep = self.keep(crumbs);
        if keep.is_none()
            && self.needle.is_empty()
            && node.children().is_empty()
            && self.rule.as_ref().is_some_and(|rule| rule.passes(node))
        {
            // Only a kept directory holds a passing file, so the parent
            // that left this one unrecorded was partial, not absent.
            return Some(Keep::Whole);
        }
        keep
    }

    /// The value a kept node is laid out by.
    pub const fn value(keep: Keep, node: Node<'_>, metric: Metric) -> u64 {
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

/// Search `node`, found at absolute `base`, for what `rule` keeps and, when
/// the needle is not blank, for names containing it. `None` when neither
/// filters anything.
pub fn filter_with(
    node: Node<'_>,
    base: &[usize],
    needle: &str,
    rule: Option<&Rule>,
) -> Option<Matches> {
    let Some(rule) = rule.filter(|rule| !rule.is_empty()) else {
        return filter(node, base, needle);
    };
    let mut matches = Matches {
        needle: needle.trim().to_ascii_lowercase(),
        base: base.to_vec(),
        rule: Some(rule.clone()),
        ..Matches::default()
    };
    let named = matches.needle.is_empty();
    let mut crumbs = base.to_vec();
    let mut kept = Vec::new();
    let tally =
        visit_rule(node, rule, &mut crumbs, &mut matches, &mut kept, named);
    matches.keep = kept.into_iter().collect();
    matches.bytes = tally.bytes;
    matches.files = tally.files;
    Some(matches)
}

/// What a rule kept beneath a node.
#[derive(Clone, Copy, Default)]
struct Tally {
    bytes: u64,
    files: u64,
    /// Something passed, even if it weighs nothing.
    any: bool,
    /// Everything passed.
    all: bool,
}

/// [`visit`] under a rule. Entries go on `kept` in depth-first order, so a
/// directory that turns out whole drops its descendants' entries by
/// truncating: they are a run at the end. `named` is set when the needle is
/// blank or an ancestor matched it.
fn visit_rule(
    node: Node<'_>,
    rule: &Rule,
    crumbs: &mut Vec<usize>,
    matches: &mut Matches,
    kept: &mut Vec<(Vec<usize>, Keep)>,
    named: bool,
) -> Tally {
    let mut total = Tally {
        all: true,
        ..Tally::default()
    };
    for (index, child) in node.children().iter().enumerate() {
        crumbs.push(index);
        let hit = !matches.needle.is_empty()
            && contains_ignoring_case(child.name(), &matches.needle);
        let child_named = named || hit;
        let tally = if child.children().is_empty() {
            let passes = child_named && rule.passes(child);
            // Without a needle a passing file is judged again when asked
            // (`Matches::keep_at`); with one it must be recorded.
            if passes && !matches.needle.is_empty() {
                kept.push((crumbs.clone(), Keep::Whole));
            }
            Tally {
                bytes: if passes { child.bytes } else { 0 },
                files: if passes { child.files } else { 0 },
                any: passes,
                all: passes,
            }
        } else {
            let start = kept.len();
            let tally =
                visit_rule(child, rule, crumbs, matches, kept, child_named);
            if tally.all {
                kept.truncate(start);
                kept.push((crumbs.clone(), Keep::Whole));
            } else if tally.any {
                kept.push((
                    crumbs.clone(),
                    Keep::Partial {
                        bytes: tally.bytes,
                        files: tally.files,
                    },
                ));
            }
            tally
        };
        if hit && !named && tally.any {
            matches.count += 1;
        }
        total.bytes += tally.bytes;
        total.files += tally.files;
        total.any |= tally.any;
        total.all &= tally.all;
        crumbs.pop();
    }
    total
}

/// Returns the bytes and files that matched at or beneath `node`'s
/// children, recording what to keep.
fn visit(
    node: Node<'_>,
    crumbs: &mut Vec<usize>,
    matches: &mut Matches,
) -> (u64, u64) {
    let mut total = (0, 0);
    for (index, child) in node.children().iter().enumerate() {
        crumbs.push(index);
        if contains_ignoring_case(child.name(), &matches.needle) {
            matches.keep.insert(crumbs.clone(), Keep::Whole);
            matches.count += 1;
            total.0 += child.bytes;
            total.1 += child.files;
        } else if !child.children().is_empty() {
            let count_before = matches.count;
            let (bytes, files) = visit(child, crumbs, matches);
            // An empty matching directory still needs a reachable ancestor.
            if matches.count > count_before {
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
    find_ignoring_case(haystack, lower_needle).is_some()
}

/// Where `lower_needle` first occurs in `haystack`, ignoring ASCII case, as
/// a byte range. The same rule [`filter`] matches by, so a label can mark
/// the part of a name that matched.
///
/// The range always falls on character boundaries: only ASCII bytes are
/// folded, and a needle that is itself valid UTF-8 cannot start or end
/// inside another character's bytes.
pub fn find_ignoring_case(
    haystack: &str,
    lower_needle: &str,
) -> Option<std::ops::Range<usize>> {
    let (hay, needle) = (haystack.as_bytes(), lower_needle.as_bytes());
    if needle.is_empty() {
        return Some(0..0);
    }
    if needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len())
        .position(|window| {
            window
                .iter()
                .zip(needle)
                .all(|(left, right)| left.to_ascii_lowercase() == *right)
        })
        .map(|start| start..start + needle.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Draft, Tree};
    use crate::tree::{NodeKind, aggregate};

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
        let mut root = Tree::from(root);
        aggregate(&mut root, Metric::Bytes);
        root
    }

    fn crumbs_of(root: Node<'_>, names: &[&str]) -> Vec<usize> {
        let mut node = root;
        names
            .iter()
            .map(|name| {
                let index = node
                    .children()
                    .iter()
                    .position(|child| child.name() == *name)
                    .expect("present");
                node = node.children().at(index);
                index
            })
            .collect()
    }

    #[test]
    fn matches_are_kept_whole_and_their_ancestors_by_what_matched() {
        let tree = tree();
        let root = tree.root();
        let found = filter(root, &[], "APP").expect("a needle");
        assert_eq!(found.count, 2, "src/App and apps; apple is inside apps");
        assert_eq!(found.bytes, 10 + 107);
        assert_eq!(found.keep(&crumbs_of(root, &["apps"])), Some(Keep::Whole));
        assert_eq!(
            found.keep(&crumbs_of(root, &["apps", "x"])),
            Some(Keep::Whole),
            "inside a match"
        );
        assert_eq!(
            found.keep(&crumbs_of(root, &["src"])),
            Some(Keep::Partial {
                bytes: 10,
                files: 1
            })
        );
        assert_eq!(found.keep(&crumbs_of(root, &["src", "notes"])), None);
        assert_eq!(found.keep(&crumbs_of(root, &["readme"])), None);
    }

    #[test]
    fn a_search_below_the_root_leaves_the_rest_alone() {
        let tree = tree();
        let root = tree.root();
        let src = crumbs_of(root, &["src"]);
        let found = filter(root.resolve(&src).expect("src"), &src, "main")
            .expect("needle");
        assert_eq!(found.keep(&crumbs_of(root, &["apps"])), Some(Keep::Whole));
        assert_eq!(found.keep(&crumbs_of(root, &["src", "notes"])), None);
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
    fn a_nested_empty_directory_keeps_its_ancestors() {
        let root = dir(
            "root",
            vec![
                dir("parent", vec![dir("empty-match", vec![])]),
                file("unrelated", 10),
            ],
        );
        let mut root = Tree::from(root);
        aggregate(&mut root, Metric::Bytes);
        let root = root.root();
        let found = filter(root, &[], "empty-match").expect("needle");
        let parent = crumbs_of(root, &["parent"]);
        let empty = crumbs_of(root, &["parent", "empty-match"]);

        assert_eq!(found.count, 1);
        assert_eq!((found.bytes, found.files), (0, 0));
        assert_eq!(
            found.keep(&parent),
            Some(Keep::Partial { bytes: 0, files: 0 })
        );
        assert_eq!(found.keep(&empty), Some(Keep::Whole));
        assert_eq!(found.keep(&crumbs_of(root, &["unrelated"])), None);
    }

    /// A file of `category`, reclaimable or not, written `days` ago.
    fn kind(
        name: &str,
        bytes: u64,
        category: Category,
        reclaim: bool,
        days: i64,
    ) -> Draft {
        let mut node = file(name, bytes);
        node.category = category;
        node.reclaim = reclaim.then_some(crate::classify::Reclaim::BuildOutput);
        node.modified = NOW - days * 86_400;
        node
    }

    const NOW: i64 = 1_800_000_000;

    /// `src` is all code (its `target` reclaimable), `mixed` holds code
    /// beside a photo, and `photos` holds only media.
    fn kinds() -> Tree {
        let root = dir(
            "root",
            vec![
                dir(
                    "src",
                    vec![
                        kind("main.rs", 10, Category::Code, false, 1),
                        dir(
                            "target",
                            vec![kind("app", 40, Category::Code, true, 1)],
                        ),
                    ],
                ),
                dir(
                    "mixed",
                    vec![
                        kind("lib.rs", 5, Category::Code, false, 400),
                        kind("cat.jpg", 20, Category::Media, false, 400),
                    ],
                ),
                dir(
                    "photos",
                    vec![kind("dog.jpg", 100, Category::Media, false, 3)],
                ),
            ],
        );
        let mut root = Tree::from(root);
        aggregate(&mut root, Metric::Bytes);
        root
    }

    fn rule() -> Rule {
        Rule {
            age_limits: vec![7, 30, 182, 365, i64::MAX],
            now: NOW,
            ..Rule::default()
        }
    }

    /// Every chip's share is what that chip alone, set to only, would
    /// show: the same files, judged by the same facts.
    #[test]
    fn shares_are_what_each_chip_would_show() {
        let tree = kinds();
        let root = tree.root();
        let base = rule();
        let shares = shares(root, NOW, &base.age_limits);
        for category in [Category::Code, Category::Media, Category::Cache] {
            let only = Rule {
                only: Rule::bit(category),
                ..base.clone()
            };
            let shown = filter_with(root, &[], "", Some(&only))
                .map_or(0, |found| found.bytes);
            assert_eq!(shares.kind(category), shown, "{category:?}");
        }
        assert_eq!(shares.kind(Category::Code), 55);
        assert_eq!(shares.kind(Category::Media), 120);
        assert_eq!(shares.suggested, 40);
        let suggested = Rule {
            reclaimable: Some(true),
            ..base
        };
        assert_eq!(
            filter_with(root, &[], "", Some(&suggested)).map(|f| f.bytes),
            Some(shares.suggested)
        );
        // This week: main.rs, app and dog.jpg; over a year: the rest.
        assert_eq!(shares.ages, vec![150, 0, 0, 0, 25]);
    }

    /// How `names` takes part, asked the way a layout asks.
    fn kept(found: &Matches, root: Node<'_>, names: &[&str]) -> Option<Keep> {
        let crumbs = crumbs_of(root, names);
        found.keep_at(&crumbs, root.resolve(&crumbs).expect("present"))
    }

    #[test]
    fn a_kind_keeps_its_files_and_sizes_the_folders_that_hold_them() {
        let tree = kinds();
        let root = tree.root();
        let code = Rule {
            only: Rule::bit(Category::Code),
            ..rule()
        };
        let found = filter_with(root, &[], "", Some(&code)).expect("a rule");
        assert_eq!((found.bytes, found.files), (55, 3));
        assert_eq!(kept(&found, root, &["src"]), Some(Keep::Whole));
        assert_eq!(
            kept(&found, root, &["mixed"]),
            Some(Keep::Partial { bytes: 5, files: 1 })
        );
        assert_eq!(kept(&found, root, &["mixed", "lib.rs"]), Some(Keep::Whole));
        assert_eq!(kept(&found, root, &["mixed", "cat.jpg"]), None);
        assert_eq!(kept(&found, root, &["photos"]), None);
        assert_eq!(found.count, 0, "a rule alone counts no names");
        // Files of a whole folder, and of a partial one, are not recorded.
        assert_eq!(found.keep.len(), 2, "{:?}", found.keep);
    }

    #[test]
    fn a_hidden_kind_leaves_everything_else_whole() {
        let tree = kinds();
        let root = tree.root();
        let no_media = Rule {
            hidden: Rule::bit(Category::Media),
            ..rule()
        };
        let found = filter_with(root, &[], "", Some(&no_media)).expect("rule");
        assert_eq!(found.bytes, 55);
        assert_eq!(kept(&found, root, &["src"]), Some(Keep::Whole));
        assert_eq!(kept(&found, root, &["src", "target"]), Some(Keep::Whole));
        assert_eq!(kept(&found, root, &["photos"]), None);
    }

    #[test]
    fn reclaimable_narrows_the_kinds_instead_of_adding_to_them() {
        let tree = kinds();
        let root = tree.root();
        let both = Rule {
            only: Rule::bit(Category::Code) | Rule::bit(Category::Media),
            reclaimable: Some(true),
            ..rule()
        };
        let found = filter_with(root, &[], "", Some(&both)).expect("rule");
        assert_eq!(found.bytes, 40, "only src/target is reclaimable");
        assert_eq!(kept(&found, root, &["src", "target"]), Some(Keep::Whole));
        assert_eq!(kept(&found, root, &["src", "main.rs"]), None);
        assert_eq!(kept(&found, root, &["mixed"]), None);

        let kept_back = Rule {
            reclaimable: Some(false),
            ..rule()
        };
        let found = filter_with(root, &[], "", Some(&kept_back)).expect("rule");
        assert_eq!(found.bytes, 135);
    }

    #[test]
    fn ages_filter_by_when_each_file_was_written() {
        let tree = kinds();
        let root = tree.root();
        let old = Rule {
            ages_only: 1 << 4,
            ..rule()
        };
        let found = filter_with(root, &[], "", Some(&old)).expect("rule");
        assert_eq!(found.bytes, 25, "only mixed's files are over a year old");
        assert_eq!(kept(&found, root, &["mixed"]), Some(Keep::Whole));

        let not_this_week = Rule {
            ages_hidden: 1,
            ..rule()
        };
        let found =
            filter_with(root, &[], "", Some(&not_this_week)).expect("rule");
        assert_eq!(found.bytes, 25);
    }

    #[test]
    fn a_needle_and_a_rule_keep_only_what_both_allow() {
        let tree = kinds();
        let root = tree.root();
        let code = Rule {
            only: Rule::bit(Category::Code),
            ..rule()
        };
        let found = filter_with(root, &[], "i", Some(&code)).expect("both");
        // "mixed" matches the name, but only its code passes the rule;
        // "main.rs" matches too. "photos" holds no code.
        assert_eq!(found.count, 2);
        assert_eq!(found.bytes, 15);
        assert_eq!(
            kept(&found, root, &["mixed"]),
            Some(Keep::Partial { bytes: 5, files: 1 })
        );
        assert_eq!(kept(&found, root, &["mixed", "lib.rs"]), Some(Keep::Whole));
        assert_eq!(kept(&found, root, &["mixed", "cat.jpg"]), None);
        assert_eq!(kept(&found, root, &["src", "main.rs"]), Some(Keep::Whole));
        assert_eq!(kept(&found, root, &["src", "target"]), None);
    }

    #[test]
    fn an_empty_rule_is_the_plain_search() {
        let tree = kinds();
        assert!(filter_with(tree.root(), &[], "", Some(&rule())).is_none());
        assert!(filter_with(tree.root(), &[], " ", None).is_none());
    }

    #[test]
    fn case_is_ignored_without_allocating() {
        assert!(contains_ignoring_case("Cargo.TOML", "toml"));
        assert!(!contains_ignoring_case("ab", "abc"));
        assert!(contains_ignoring_case("x", ""));
    }

    /// A label underlines exactly what matched, so the range has to land on
    /// the matched bytes, characters before it included.
    #[test]
    fn a_match_is_found_where_it_is_in_the_name() {
        assert_eq!(find_ignoring_case("build-CACHE", "cache"), Some(6..11));
        // "é" is two bytes: the range counts bytes, and stays on boundaries.
        let name = "Résumé cache.txt";
        let range = find_ignoring_case(name, "cache").expect("it matches");
        assert_eq!(&name[range], "cache");
        assert_eq!(find_ignoring_case(name, "é c"), Some(6..10));
        assert_eq!(find_ignoring_case("obj", "cache"), None);
    }
}

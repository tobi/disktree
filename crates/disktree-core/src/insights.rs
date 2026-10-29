//! "Worth a look": the biggest things in a scan that could plausibly go.
//!
//! Three kinds of finding, each one a directory a person can judge in a
//! second: reclaimable space ([`crate::classify::Reclaim`]), agent worktrees,
//! and experiments nobody has written to in a month. Findings never nest, so
//! their total is space that really exists once.

use crate::classify::{Category, Reclaim};
use crate::tree::Node;

/// Seconds in a day.
const DAY: i64 = 86_400;

/// An experiment untouched this long is worth a look.
pub const STALE_DAYS: i64 = 30;

/// Maximum findings shown by either interface.
pub const INSIGHT_LIMIT: usize = 6;

/// Smaller than this is not worth a line.
const MIN_BYTES: u64 = 64 * 1024 * 1024;

/// Why a directory is on the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Finding {
    /// Its space can be had back, for this reason.
    Reclaimable(Reclaim),
    /// A directory of agent worktrees: how many, and the oldest one's age.
    Worktrees { count: usize, oldest_days: i64 },
    /// Experiments untouched for [`STALE_DAYS`]: only those are counted.
    StaleExperiments { count: usize },
}

/// One line of the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Where it is, from the scanned root.
    pub crumbs: Vec<usize>,
    /// What clearing it frees. For stale experiments, only the stale ones.
    pub bytes: u64,
    pub finding: Finding,
}

/// The `limit` largest findings beneath `root`, largest first. `now` is Unix
/// seconds, passed in so the answer is testable.
pub fn worth_a_look(root: &Node, now: i64, limit: usize) -> Vec<Candidate> {
    let mut found = Vec::new();
    let mut crumbs = Vec::new();
    for (index, child) in root.children.iter().enumerate() {
        crumbs.push(index);
        visit(child, &mut crumbs, now, &mut found);
        crumbs.pop();
    }
    found.retain(|candidate| candidate.bytes >= MIN_BYTES);
    found.sort_by_key(|candidate| std::cmp::Reverse(candidate.bytes));
    found.truncate(limit);
    found
}

fn visit(
    node: &Node,
    crumbs: &mut Vec<usize>,
    now: i64,
    found: &mut Vec<Candidate>,
) {
    // Nothing beneath a small directory can reach `MIN_BYTES` either, so a
    // scan of millions of files looks at a few thousand directories.
    if !node.is_dir() || node.bytes < MIN_BYTES {
        return;
    }
    // Topmost only: everything beneath a reclaimable directory goes with it.
    if let Some(reason) = node.reclaim {
        found.push(Candidate {
            crumbs: crumbs.clone(),
            bytes: node.bytes,
            finding: Finding::Reclaimable(reason),
        });
        return;
    }
    let name = &*node.name;
    let scratch = node.category == Category::AgentScratch;
    if scratch && name.eq_ignore_ascii_case("worktrees") {
        let trees: Vec<&Node> = node
            .children
            .iter()
            .filter(|child| child.is_dir())
            .collect();
        if !trees.is_empty() {
            let oldest = trees
                .iter()
                .map(|tree| tree.modified)
                .filter(|&time| time > 0)
                .min()
                .unwrap_or(now);
            found.push(Candidate {
                crumbs: crumbs.clone(),
                bytes: node.bytes,
                finding: Finding::Worktrees {
                    count: trees.len(),
                    oldest_days: (now - oldest).max(0) / DAY,
                },
            });
            return;
        }
    }
    let experiments = scratch
        && (name.eq_ignore_ascii_case("tries")
            || name.eq_ignore_ascii_case("experiments"));
    let mut stale = (0_usize, 0_u64);
    for (index, child) in node.children.iter().enumerate() {
        let is_stale = experiments
            && child.is_dir()
            && child.modified > 0
            && now - child.modified > STALE_DAYS * DAY;
        if is_stale {
            stale.0 += 1;
            stale.1 += child.bytes;
            // A stale experiment is judged whole; its caches go with it.
            continue;
        }
        crumbs.push(index);
        visit(child, crumbs, now, found);
        crumbs.pop();
    }
    if stale.0 > 0 {
        found.push(Candidate {
            crumbs: crumbs.clone(),
            bytes: stale.1,
            finding: Finding::StaleExperiments { count: stale.0 },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::classify;
    use crate::tree::{Metric, NodeKind, aggregate};

    const GIB: u64 = 1024 * 1024 * 1024;
    const NOW: i64 = 1_800_000_000;

    fn file(name: &str, bytes: u64, days_old: i64) -> Node {
        let mut node = Node::entry(name, NodeKind::File, bytes);
        node.modified = NOW - days_old * DAY;
        node
    }

    fn dir(name: &str, children: Vec<Node>) -> Node {
        let mut node = Node::directory(name);
        node.children = children;
        node
    }

    fn scan(mut root: Node) -> Node {
        aggregate(&mut root, Metric::Bytes);
        classify(&mut root);
        root
    }

    fn home() -> Node {
        scan(dir(
            "tobi",
            vec![
                dir(
                    ".cache",
                    vec![dir("kache", vec![file("blob", 5 * GIB, 1)])],
                ),
                dir(
                    ".codex",
                    vec![dir(
                        "worktrees",
                        vec![
                            dir("a1", vec![file("x", 3 * GIB, 41)]),
                            dir("b2", vec![file("y", 2 * GIB, 3)]),
                        ],
                    )],
                ),
                dir(
                    "src",
                    vec![dir(
                        "tries",
                        vec![
                            dir("old", vec![file("z", 4 * GIB, 90)]),
                            dir(
                                "fresh",
                                vec![
                                    file("Cargo.toml", 1, 1),
                                    dir("target", vec![file("o", GIB, 1)]),
                                ],
                            ),
                        ],
                    )],
                ),
                dir("Documents", vec![file("tax.pdf", 9 * GIB, 400)]),
                dir("tiny", vec![dir(".cache", vec![file("t", 1024, 1)])]),
            ],
        ))
    }

    #[test]
    fn ranks_findings_largest_first_and_skips_the_tiny() {
        let found = worth_a_look(&home(), NOW, 10);
        let kinds: Vec<&Finding> = found.iter().map(|c| &c.finding).collect();
        assert_eq!(
            kinds,
            vec![
                &Finding::Reclaimable(Reclaim::Regenerable),
                &Finding::Worktrees {
                    count: 2,
                    oldest_days: 41
                },
                &Finding::StaleExperiments { count: 1 },
                &Finding::Reclaimable(Reclaim::BuildOutput),
            ]
        );
        assert_eq!(found[0].bytes, 5 * GIB);
        assert_eq!(found[2].bytes, 4 * GIB, "only the stale experiment counts");
    }

    #[test]
    fn documents_are_never_suggested() {
        let found = worth_a_look(&home(), NOW, 10);
        assert!(found.iter().all(|c| c.crumbs != vec![3]));
    }

    #[test]
    fn crumbs_address_the_finding_from_the_root() {
        let root = home();
        for candidate in worth_a_look(&root, NOW, 10) {
            let node = root.resolve(&candidate.crumbs).expect("resolves");
            assert!(node.is_dir());
        }
    }

    #[test]
    fn the_limit_keeps_the_largest() {
        let found = worth_a_look(&home(), NOW, 2);
        assert_eq!(found.len(), 2);
        assert_eq!(
            found[1].finding,
            Finding::Worktrees {
                count: 2,
                oldest_days: 41
            }
        );
    }
}

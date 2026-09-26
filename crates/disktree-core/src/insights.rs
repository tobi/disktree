//! "Worth a look": the biggest things in a scan that could plausibly go.
//!
//! Five kinds of finding, each one a thing a person can judge in a second:
//! reclaimable space ([`crate::classify::Reclaim`]), agent worktrees,
//! experiments nobody has written to in a month, directories of model
//! weights, and archives nobody has written to in a month. Findings never
//! nest, so their total is space that really exists once.
//!
//! Only reclaimable space is known to come back. Weights and archives are
//! often someone's only copy: they are on the list because they are large
//! and easy to forget, not because they are safe to delete.

use crate::classify::{Category, Reclaim};
use crate::tree::Node;

/// Seconds in a day.
const DAY: i64 = 86_400;

/// An experiment untouched this long is worth a look.
pub const STALE_DAYS: i64 = 30;

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
    /// A directory holding model weights directly: how many weight files.
    /// Only the weight files are counted.
    Weights { files: usize },
    /// An archive or disk image untouched for [`STALE_DAYS`]: a download
    /// that was unpacked, installed or read, and then left.
    StaleArchive { days: i64 },
}

/// Extensions that are model weights whatever directory they sit in.
const WEIGHT_EXTENSIONS: [&str; 8] = [
    "safetensors",
    "gguf",
    "ggml",
    "ckpt",
    "pt",
    "pth",
    "onnx",
    "tflite",
];

/// Extensions of archives and disk images. `gz`, `xz`, `zst` and `bz2`
/// cover `.tar.gz` and friends, since only the last extension is read.
const ARCHIVE_EXTENSIONS: [&str; 12] = [
    "zip", "tar", "tgz", "gz", "xz", "zst", "bz2", "7z", "rar", "dmg", "iso",
    "pkg",
];

/// The lowercased last extension of a file name, if it has one.
fn extension(name: &str) -> Option<String> {
    let (stem, extension) = name.rsplit_once('.')?;
    (!stem.is_empty()).then(|| extension.to_ascii_lowercase())
}

/// Whether `name` is a weight file. `.bin` is too common to trust on its
/// own, so it counts only in the shape a Hugging Face checkpoint has, beside
/// a `config.json`.
fn is_weight_file(name: &str, beside_config: bool) -> bool {
    extension(name).is_some_and(|extension| {
        WEIGHT_EXTENSIONS.contains(&extension.as_str())
            || (beside_config && extension == "bin")
    })
}

fn is_archive(name: &str) -> bool {
    extension(name).is_some_and(|extension| {
        ARCHIVE_EXTENSIONS.contains(&extension.as_str())
    })
}

/// Days since an archive was written, when `node` is one untouched for
/// [`STALE_DAYS`]. A sync client owns its folder, so an old archive there is
/// a backup and not a forgotten download.
fn stale_archive_age(node: &Node, now: i64) -> Option<i64> {
    let days = (now - node.modified) / DAY;
    let stale = node.category != Category::Synced
        && node.modified > 0
        && days > STALE_DAYS
        && is_archive(&node.name);
    stale.then_some(days)
}

/// The weight files directly in `node`, the ones a
/// [`Finding::Weights`] counts.
pub fn weight_files(node: &Node) -> impl Iterator<Item = &Node> {
    let beside_config = node
        .children
        .iter()
        .any(|child| &*child.name == "config.json");
    node.children.iter().filter(move |child| {
        !child.is_dir() && is_weight_file(&child.name, beside_config)
    })
}

/// The weight files directly in `node`: how many, and their bytes.
fn weights_in(node: &Node) -> (usize, u64) {
    weight_files(node).fold((0, 0), |(files, bytes), child| {
        (files + 1, bytes + child.bytes)
    })
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
    if !node.is_dir() {
        if let Some(days) = stale_archive_age(node, now) {
            found.push(Candidate {
                crumbs: crumbs.clone(),
                bytes: node.bytes,
                finding: Finding::StaleArchive { days },
            });
        }
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
    let name = node.name.to_ascii_lowercase();
    let scratch = node.category == Category::AgentScratch;
    if scratch && matches!(name.as_str(), "worktrees" | ".worktrees") {
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
    // Weights are judged by the directory that holds them: a checkpoint is
    // its shards and config together, and what sits beneath goes with it.
    let (weight_files, weight_bytes) = weights_in(node);
    if weight_bytes >= MIN_BYTES {
        found.push(Candidate {
            crumbs: crumbs.clone(),
            bytes: weight_bytes,
            finding: Finding::Weights {
                files: weight_files,
            },
        });
        return;
    }
    let experiments =
        scratch && matches!(name.as_str(), "tries" | "experiments");
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
                dir(
                    "Downloads",
                    vec![
                        file("export.zip", 6 * GIB, 60),
                        file("fresh.dmg", 6 * GIB, 2),
                    ],
                ),
                dir(
                    "lab",
                    vec![dir(
                        "models",
                        vec![
                            file("config.json", 1, 5),
                            file("pytorch_model.bin", 7 * GIB, 5),
                            file("tokenizer.model", GIB, 5),
                        ],
                    )],
                ),
                dir("Dropbox", vec![file("backup.zip", 8 * GIB, 300)]),
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
                &Finding::Weights { files: 1 },
                &Finding::StaleArchive { days: 60 },
                &Finding::Reclaimable(Reclaim::Regenerable),
                &Finding::Worktrees {
                    count: 2,
                    oldest_days: 41
                },
                &Finding::StaleExperiments { count: 1 },
                &Finding::Reclaimable(Reclaim::BuildOutput),
            ]
        );
        assert_eq!(found[0].bytes, 7 * GIB, "only the weight files count");
        assert_eq!(found[2].bytes, 5 * GIB);
        assert_eq!(found[4].bytes, 4 * GIB, "only the stale experiment counts");
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
            let is_file =
                matches!(candidate.finding, Finding::StaleArchive { .. });
            assert_eq!(node.is_dir(), !is_file);
        }
    }

    #[test]
    fn fresh_and_synced_archives_are_not_suggested() {
        let found = worth_a_look(&home(), NOW, 10);
        let archives = found
            .iter()
            .filter(|c| matches!(c.finding, Finding::StaleArchive { .. }))
            .count();
        assert_eq!(archives, 1, "only Downloads/export.zip");
    }

    #[test]
    fn a_dotted_worktrees_directory_counts() {
        let root = scan(dir(
            "home",
            vec![dir(
                "project",
                vec![dir(
                    ".worktrees",
                    vec![dir("autopilot", vec![file("x", GIB, 9)])],
                )],
            )],
        ));
        let found = worth_a_look(&root, NOW, 10);
        assert_eq!(
            found[0].finding,
            Finding::Worktrees {
                count: 1,
                oldest_days: 9
            }
        );
    }

    #[test]
    fn a_bare_bin_file_is_not_a_weight() {
        let root = scan(dir(
            "home",
            vec![dir("firmware", vec![file("image.bin", 5 * GIB, 5)])],
        ));
        assert!(worth_a_look(&root, NOW, 10).is_empty());
    }

    #[test]
    fn weights_found_by_extension_anywhere() {
        let root = scan(dir(
            "home",
            vec![dir(
                "PitchLab",
                vec![dir(
                    "models",
                    vec![
                        file("a.gguf", 4 * GIB, 5),
                        file("b.safetensors", 3 * GIB, 5),
                        file("notes.md", 1, 5),
                    ],
                )],
            )],
        ));
        let found = worth_a_look(&root, NOW, 10);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].finding, Finding::Weights { files: 2 });
        assert_eq!(found[0].bytes, 7 * GIB);
    }

    #[test]
    fn the_limit_keeps_the_largest() {
        let found = worth_a_look(&home(), NOW, 2);
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].finding, Finding::StaleArchive { days: 60 });
    }
}

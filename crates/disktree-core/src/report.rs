//! The scanned tree as text, for a terminal: `disktree --cli`.
//!
//! The same tree the window draws, largest first, for a caller that can
//! only read standard output: an agent, an SSH session, a script. Each row
//! is one entry with its value and its share of its parent, so the answer
//! to "what is eating this directory" is the first few lines.
//!
//! Something downstream reads this line by line, so a name is written
//! through the same rule as the delete list in [`crate::export`]: one that
//! holds a control character is escaped, and can never read as two rows.

use std::fmt::Write as _;

use crate::size::{human_bytes, human_count, share};
use crate::tree::{Metric, Node};

/// Children a level prints before the rest share one row. The treemap
/// keeps the same number, `LayoutOptions::max_children`, so the terminal
/// and the window agree on where a directory's tail begins.
pub const CHILDREN_KEPT: usize = 96;

/// Columns a value is right-aligned in: `1023 MiB` is the widest size, and
/// two more keep the column apart from the branch glyphs.
const VALUE_COLUMNS: usize = 10;

/// `tree` as an indented listing, `depth` levels below the root, which is
/// named `label`. Children are in the order [`crate::tree::aggregate`] left
/// them: ranked by `metric`, descending.
pub fn text_tree(
    label: &str,
    tree: &Node,
    metric: Metric,
    depth: u32,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}  {}", value(tree, metric), one_line(label));
    rows(&mut out, tree, metric, depth.max(1), "");
    let _ = write!(
        out,
        "\n{} in {} files, {} directories",
        human_bytes(tree.bytes),
        human_count(tree.files),
        human_count(u64::from(tree.dirs)),
    );
    let unreadable = unreadable(tree);
    if unreadable > 0 {
        let _ = write!(out, " \u{2014} {unreadable} unreadable");
    }
    out.push('\n');
    out
}

/// The rows for `node`'s children, and theirs while `depth` lasts.
fn rows(out: &mut String, node: &Node, metric: Metric, depth: u32, lead: &str) {
    let total = node.value(metric);
    let kept = node.children.len().min(CHILDREN_KEPT);
    let (shown, rest) = node.children.split_at(kept);
    for (index, child) in shown.iter().enumerate() {
        let last = rest.is_empty() && index + 1 == shown.len();
        let (branch, below) = if last {
            ("\u{2514}\u{2500}\u{2500} ", "    ")
        } else {
            ("\u{251c}\u{2500}\u{2500} ", "\u{2502}   ")
        };
        // A directory the depth limit stops at is not empty, and must not
        // read as if it were.
        let cut = depth <= 1 && !child.children.is_empty();
        let _ = writeln!(
            out,
            "{lead}{branch}{:>VALUE_COLUMNS$}  {:>3.0}%  {}{}{}",
            value(child, metric),
            share(child.value(metric), total),
            one_line(&child.name),
            if child.is_dir() { "/" } else { "" },
            if cut { " \u{2026}" } else { "" },
        );
        if depth > 1 && !child.children.is_empty() {
            rows(out, child, metric, depth - 1, &format!("{lead}{below}"));
        }
    }
    if rest.is_empty() {
        return;
    }
    let merged = rest
        .iter()
        .fold(0_u64, |sum, child| sum.saturating_add(child.value(metric)));
    let _ = writeln!(
        out,
        "{lead}\u{2514}\u{2500}\u{2500} {:>VALUE_COLUMNS$}  {:>3.0}%  \
         \u{2026} {} more",
        amount(merged, metric),
        share(merged, total),
        rest.len(),
    );
}

fn value(node: &Node, metric: Metric) -> String {
    amount(node.value(metric), metric)
}

fn amount(value: u64, metric: Metric) -> String {
    match metric {
        Metric::Bytes => human_bytes(value),
        Metric::Files => human_count(value),
    }
}

/// Directories that could not be read, anywhere in the tree. Iterative: a
/// tree is as deep as the deepest path on the disk.
fn unreadable(tree: &Node) -> u64 {
    let mut count = 0;
    let mut pending = vec![tree];
    while let Some(node) = pending.pop() {
        count += u64::from(node.read_error);
        pending.extend(&node.children);
    }
    count
}

/// `name` with every control character escaped, so it stays on its row.
fn one_line(name: &str) -> String {
    if !name.chars().any(char::is_control) {
        return name.to_owned();
    }
    name.chars()
        .map(|char| {
            if char.is_control() {
                char.escape_default().to_string()
            } else {
                char.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::NodeKind;

    fn file(name: &str, bytes: u64) -> Node {
        Node::entry(name, NodeKind::File, bytes)
    }

    fn dir(name: &str, children: Vec<Node>, metric: Metric) -> Node {
        let mut node = Node::directory(name);
        node.children = children;
        crate::tree::aggregate(&mut node, metric);
        node
    }

    const GIB: u64 = 1 << 30;

    fn sample(metric: Metric) -> Node {
        dir(
            "work",
            vec![
                file("small.txt", GIB),
                dir(
                    "builds",
                    vec![file("debug", 3 * GIB), file("release", 9 * GIB)],
                    metric,
                ),
                file("data.bin", 4 * GIB),
            ],
            metric,
        )
    }

    #[test]
    fn rows_are_largest_first_with_their_share_of_the_parent() {
        let text = text_tree(
            "/home/me/work",
            &sample(Metric::Bytes),
            Metric::Bytes,
            3,
        );
        let expected = "\
17 GiB  /home/me/work
\u{251c}\u{2500}\u{2500}     12 GiB   71%  builds/
\u{2502}   \u{251c}\u{2500}\u{2500}    9.0 GiB   75%  release
\u{2502}   \u{2514}\u{2500}\u{2500}    3.0 GiB   25%  debug
\u{251c}\u{2500}\u{2500}    4.0 GiB   24%  data.bin
\u{2514}\u{2500}\u{2500}    1.0 GiB    6%  small.txt

17 GiB in 4 files, 2 directories
";
        assert_eq!(text, expected);
    }

    #[test]
    fn a_directory_past_the_depth_limit_is_marked_not_shown_empty() {
        let text = text_tree("work", &sample(Metric::Bytes), Metric::Bytes, 1);
        assert!(text.contains("builds/ \u{2026}"), "{text}");
        assert!(!text.contains("release"), "{text}");
    }

    #[test]
    fn the_file_metric_ranks_and_prints_counts() {
        let text = text_tree("work", &sample(Metric::Files), Metric::Files, 1);
        let first = text.lines().nth(1).expect("a row");
        assert!(first.contains("builds/"), "{text}");
        assert!(first.contains("  2   50%"), "{text}");
    }

    #[test]
    fn the_tail_past_the_kept_children_is_one_summed_row() {
        let children = (0..CHILDREN_KEPT + 4)
            .map(|index| file(&format!("f{index}"), 1024))
            .collect();
        let text = text_tree(
            "many",
            &dir("many", children, Metric::Bytes),
            Metric::Bytes,
            1,
        );
        // The root line, the kept rows, the merged row, a blank, the total.
        assert_eq!(text.lines().count(), CHILDREN_KEPT + 4);
        assert!(text.contains("4.0 KiB    4%  \u{2026} 4 more"), "{text}");
    }

    #[test]
    fn a_name_with_a_line_break_stays_on_its_row() {
        let tree = dir(
            "root",
            vec![file("one\ntwo", 10), file("plain", 5)],
            Metric::Bytes,
        );
        let text = text_tree("root", &tree, Metric::Bytes, 1);
        assert!(text.contains("one\\ntwo"), "{text}");
        assert_eq!(text.lines().count(), 5, "{text}");
    }

    #[test]
    fn unreadable_directories_are_counted_in_the_total_line() {
        let mut locked = Node::directory("locked");
        locked.read_error = true;
        let tree = dir("root", vec![locked, file("a", 10)], Metric::Bytes);
        let text = text_tree("root", &tree, Metric::Bytes, 2);
        assert!(text.ends_with("\u{2014} 1 unreadable\n"), "{text}");
    }
}

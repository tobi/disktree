//! A finished scan as a report for a terminal or another program.
//!
//! The largest entries as an indented tree, what kind of data the scan holds
//! and how much of it can be had back, and what is worth a look, with the
//! volume's free space beside them.
//!
//! It reads the classification and findings the window draws, so
//! `disktree report` and the treemap never disagree about what is
//! reclaimable. Nothing here touches the filesystem: a report is a walk over
//! a tree that is already finished.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::classify::{Category, Reclaim};
use crate::insights::{self, Finding, STALE_DAYS};
use crate::size::{human_bytes, human_count, parse_bytes, share};
use crate::space::SpaceInfo;
use crate::tree::{Metric, Node, NodeKind, path_of};

/// The smallest entry the tree lists. Smaller ones are summed into their
/// directory's remainder line, so no total loses anything.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Threshold {
    /// At least this many bytes.
    Bytes(u64),
    /// At least this percentage of the whole scan, by the ranked measure.
    Share(f64),
}

impl Threshold {
    /// `512M`, `2G`, `1.5GiB`, a plain byte count, or a share such as `1%`.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if let Some(number) = text.strip_suffix('%') {
            let percent: f64 = number.trim_end().parse().ok()?;
            // `contains` is false for NaN too.
            return (0.0..=100.0)
                .contains(&percent)
                .then_some(Self::Share(percent));
        }
        parse_bytes(text).map(Self::Bytes)
    }

    fn admits(self, node: &Node, total: u64, metric: Metric) -> bool {
        match self {
            Self::Bytes(bytes) => node.bytes >= bytes,
            Self::Share(percent) => {
                node.value(metric) as f64 >= total as f64 * percent / 100.0
            }
        }
    }
}

/// How much of the scan a report lists.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Levels of the tree below the root.
    pub depth: usize,
    /// Entries listed in each directory, largest first.
    pub top: usize,
    pub min: Threshold,
    /// "Worth a look" findings listed, largest first.
    pub findings: usize,
}

impl Default for Limits {
    /// Three levels, like the window draws at first. Ten entries and a
    /// one percent floor keep a home directory to a screen or two; the
    /// remainder lines say how much was folded away.
    fn default() -> Self {
        Self {
            depth: 3,
            top: 10,
            min: Threshold::Share(1.0),
            findings: 20,
        }
    }
}

/// What the caller measured besides the tree.
#[derive(Clone, Debug, Default)]
pub struct Scanned {
    /// The scanned root, absolute.
    pub root: PathBuf,
    /// Written as `~` in text, so paths stay short.
    pub home: Option<PathBuf>,
    pub metric: Metric,
    pub apparent_size: bool,
    /// Paths the walk could not read, and the first of them verbatim.
    pub unreadable: u64,
    pub unreadable_paths: Vec<String>,
    /// The device the root is mounted from, and its space, when readable.
    pub device: Option<String>,
    pub space: Option<SpaceInfo>,
    /// `Some(false)` when macOS is known to be hiding folders.
    pub full_disk_access: Option<bool>,
    /// Unix seconds, for how old a finding is; passed in to be testable.
    pub now: i64,
}

/// A scan, summarised. Serialises as the `--json` document.
#[derive(Debug, Serialize)]
pub struct Report {
    /// The scanned root, absolute.
    pub root: String,
    /// `"disk usage"`, the blocks deleting gives back, or
    /// `"apparent size"`, the length `ls -l` shows.
    pub measure: &'static str,
    /// `"size"` or `"files"`: what the tree is ordered and cut by.
    pub ranked_by: &'static str,
    pub bytes: u64,
    pub files: u64,
    pub dirs: u64,
    /// Paths the walk could not read; what they hold is not counted.
    pub unreadable: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unreadable_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_disk_access: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<Volume>,
    /// The whole scan by kind of data, largest first.
    pub kinds: Vec<KindTotal>,
    pub reclaimable: Reclaimable,
    /// The window's "worth a look" list.
    pub findings: Vec<Suggestion>,
    pub tree: Entry,
    #[serde(skip)]
    home: Option<PathBuf>,
    #[serde(skip)]
    metric: Metric,
}

/// The volume the root is on.
#[derive(Debug, Serialize)]
pub struct Volume {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(flatten)]
    pub space: SpaceInfo,
}

/// One kind of data, over the whole scan.
#[derive(Debug, Serialize)]
pub struct KindTotal {
    pub category: Category,
    pub label: &'static str,
    pub bytes: u64,
    pub files: u64,
}

/// Space the window hatches: caches, build output, package stores and the
/// rest of [`Reclaim`], over the whole scan.
#[derive(Debug, Default, Serialize)]
pub struct Reclaimable {
    pub bytes: u64,
    /// By reason, largest first.
    pub reasons: Vec<ReasonTotal>,
}

#[derive(Debug, Serialize)]
pub struct ReasonTotal {
    pub reclaim: Reclaim,
    pub label: &'static str,
    pub bytes: u64,
}

/// One line of "worth a look".
#[derive(Debug, Serialize)]
pub struct Suggestion {
    pub path: String,
    /// What clearing it frees. For stale experiments, only the stale ones.
    pub bytes: u64,
    /// `"reclaimable"`, `"worktrees"` or `"stale_experiments"`.
    pub finding: &'static str,
    /// Why it is on the list, as the window says it.
    pub why: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reclaim: Option<Reclaim>,
}

/// One entry of the tree, with the largest of its children.
#[derive(Debug, Serialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    /// `"directory"`, `"file"`, `"symlink"` or `"other"`.
    pub kind: &'static str,
    pub bytes: u64,
    pub files: u64,
    pub dirs: u32,
    /// Newest write at or beneath it, in Unix seconds; `0` when unknown.
    pub modified: i64,
    pub category: Category,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reclaim: Option<Reclaim>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub unreadable: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<Self>,
    /// What the limits left out of `children`, summed. Absent at the depth
    /// limit, where no child is listed and the entry's totals say it all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rest: Option<Rest>,
}

/// Entries a directory holds that its listing leaves out.
#[derive(Debug, Default, Serialize)]
pub struct Rest {
    pub entries: u64,
    pub bytes: u64,
    pub files: u64,
}

impl Report {
    pub fn new(tree: &Node, scanned: Scanned, limits: Limits) -> Self {
        let total = tree.value(scanned.metric);
        let admits =
            |node: &Node| limits.min.admits(node, total, scanned.metric);
        let mut outline = Outline {
            depth: limits.depth,
            top: limits.top,
            admits: &admits,
        }
        .entry(tree, &scanned.root);
        outline.name = outline.path.clone();
        let (kinds, reclaimable) = tally(tree);
        let findings =
            insights::worth_a_look(tree, scanned.now, limits.findings)
                .into_iter()
                .map(|candidate| Suggestion {
                    path: path_of(&scanned.root, tree, &candidate.crumbs)
                        .to_string_lossy()
                        .into_owned(),
                    bytes: candidate.bytes,
                    finding: match candidate.finding {
                        Finding::Reclaimable(_) => "reclaimable",
                        Finding::Worktrees { .. } => "worktrees",
                        Finding::StaleExperiments { .. } => "stale_experiments",
                    },
                    why: why(&candidate.finding),
                    reclaim: match candidate.finding {
                        Finding::Reclaimable(reason) => Some(reason),
                        _ => None,
                    },
                })
                .collect();
        Self {
            root: scanned.root.to_string_lossy().into_owned(),
            measure: if scanned.apparent_size {
                "apparent size"
            } else {
                "disk usage"
            },
            ranked_by: scanned.metric.label(),
            bytes: tree.bytes,
            files: tree.files,
            dirs: u64::from(tree.dirs),
            unreadable: scanned.unreadable,
            unreadable_paths: scanned.unreadable_paths,
            full_disk_access: scanned.full_disk_access,
            volume: scanned.space.map(|space| Volume {
                device: scanned.device,
                space,
            }),
            kinds,
            reclaimable,
            findings,
            tree: outline,
            home: scanned.home,
            metric: scanned.metric,
        }
    }

    /// The `--json` document.
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// The report for a terminal: totals, the tree, the kinds of data, and
    /// what is worth a look.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let measure = if self.measure == "disk usage" {
            String::new()
        } else {
            format!(" ({})", self.measure)
        };
        let _ = write!(
            out,
            "{}  {}{measure} · {} files · {} dirs",
            self.shown(&self.root),
            human_bytes(self.bytes),
            human_count(self.files),
            human_count(self.dirs),
        );
        if self.unreadable > 0 {
            let _ =
                write!(out, " · {} unreadable", human_count(self.unreadable));
        }
        out.push('\n');
        if let Some(volume) = &self.volume {
            let _ = writeln!(
                out,
                "{}  {} free of {} · {:.0}% used",
                volume.device.as_deref().unwrap_or("volume"),
                human_bytes(volume.space.available),
                human_bytes(volume.space.total),
                volume.space.used_fraction() * 100.0,
            );
        }

        out.push_str("\n    size  share    files\n");
        let total = self.ranked(self.tree.bytes, self.tree.files);
        self.rows(&mut out, &self.tree, 0, total);

        if !self.kinds.is_empty() {
            out.push_str("\nBy kind\n");
            for kind in &self.kinds {
                let _ = writeln!(
                    out,
                    "{:>8}  {:>5}  {}",
                    human_bytes(kind.bytes),
                    percent(kind.bytes, self.bytes),
                    kind.label,
                );
            }
        }
        if self.reclaimable.bytes > 0 {
            let _ = writeln!(
                out,
                "\nCan be had back: {}, {} of the scan (hatched in the \
                 window)",
                human_bytes(self.reclaimable.bytes),
                percent(self.reclaimable.bytes, self.bytes).trim_start(),
            );
            for reason in &self.reclaimable.reasons {
                let _ = writeln!(
                    out,
                    "{:>8}  {}",
                    human_bytes(reason.bytes),
                    reason.label
                );
            }
        }
        if !self.findings.is_empty() {
            out.push_str("\nWorth a look\n");
            let width = self
                .findings
                .iter()
                .map(|found| found.why.chars().count())
                .max()
                .unwrap_or(0);
            for found in &self.findings {
                let _ = writeln!(
                    out,
                    "{:>8}  {:<width$}  {}",
                    human_bytes(found.bytes),
                    found.why,
                    self.shown(&found.path),
                );
            }
        }
        if self.unreadable > 0 {
            let noun = if self.unreadable == 1 {
                "path"
            } else {
                "paths"
            };
            let _ = writeln!(
                out,
                "\n{} {noun} could not be read; what they hold is not \
                 counted.",
                human_count(self.unreadable),
            );
            if self.full_disk_access == Some(false) {
                out.push_str(
                    "macOS keeps them from this terminal. Give it Full Disk \
                     Access in System Settings › Privacy & Security, then \
                     run this again.\n",
                );
            }
        }
        out
    }

    /// The tree's rows beneath `entry`, `level` deep.
    fn rows(&self, out: &mut String, entry: &Entry, level: usize, total: u64) {
        let indent = "  ".repeat(level);
        for child in &entry.children {
            let slash = if child.kind == "directory" { "/" } else { "" };
            let _ = writeln!(
                out,
                "{:>8}  {:>5}  {:>7}  {indent}{}{slash}{}",
                human_bytes(child.bytes),
                percent(self.ranked(child.bytes, child.files), total),
                human_count(child.files),
                printable(&child.name),
                tag(entry, child),
            );
            self.rows(out, child, level + 1, total);
        }
        if let Some(rest) = &entry.rest {
            let noun = if rest.entries == 1 { "other" } else { "others" };
            let _ = writeln!(
                out,
                "{:>8}  {:>5}  {:>7}  {indent}… {} {noun}",
                human_bytes(rest.bytes),
                percent(self.ranked(rest.bytes, rest.files), total),
                human_count(rest.files),
                human_count(rest.entries),
            );
        }
    }

    /// Of an entry's two totals, the one the tree is ranked by.
    const fn ranked(&self, bytes: u64, files: u64) -> u64 {
        match self.metric {
            Metric::Bytes => bytes,
            Metric::Files => files,
        }
    }

    /// `path` for reading: under the home directory as `~`, and with any
    /// control character escaped so a name cannot forge another line.
    fn shown(&self, path: &str) -> String {
        let path = Path::new(path);
        let shown = match self
            .home
            .as_deref()
            .and_then(|home| path.strip_prefix(home).ok())
        {
            Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
            Some(rest) => {
                format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display())
            }
            None => path.display().to_string(),
        };
        printable(&shown).into_owned()
    }
}

/// Builds the listed tree.
struct Outline<'a> {
    depth: usize,
    top: usize,
    admits: &'a dyn Fn(&Node) -> bool,
}

impl Outline<'_> {
    /// `node`, at `path`, with its largest children down to the depth
    /// limit. Children arrive ordered by the ranked measure, largest first.
    fn entry(&self, node: &Node, path: &Path) -> Entry {
        let mut entry = Entry {
            name: node.name.to_string(),
            path: path.to_string_lossy().into_owned(),
            kind: match node.kind {
                NodeKind::Directory => "directory",
                NodeKind::File => "file",
                NodeKind::Symlink => "symlink",
                NodeKind::Other => "other",
            },
            bytes: node.bytes,
            files: node.files,
            dirs: node.dirs,
            modified: node.modified,
            category: node.category,
            reclaim: node.reclaim,
            unreadable: node.read_error,
            children: Vec::new(),
            rest: None,
        };
        if !node.is_dir() || self.depth == 0 {
            return entry;
        }
        let below = Self {
            depth: self.depth - 1,
            ..*self
        };
        let mut rest = Rest::default();
        for child in &node.children {
            if entry.children.len() < self.top && (self.admits)(child) {
                entry
                    .children
                    .push(below.entry(child, &path.join(&*child.name)));
            } else {
                rest.entries += 1;
                rest.bytes = rest.bytes.saturating_add(child.bytes);
                rest.files += child.files;
            }
        }
        entry.rest = (rest.entries > 0).then_some(rest);
        entry
    }
}

/// Every byte in the scan by kind, and what of it can be had back. A leaf
/// carries its directory's kind and reclaim reason, so summing the leaves
/// counts each byte once, hardlinks included: a second name weighs nothing.
fn tally(root: &Node) -> (Vec<KindTotal>, Reclaimable) {
    let mut kinds: Vec<KindTotal> = Vec::new();
    let mut reclaimable = Reclaimable::default();
    // A stack rather than recursion: a tree can be deeper than a thread's
    // stack is tall.
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_dir() {
            stack.extend(&node.children);
            continue;
        }
        match kinds.iter_mut().find(|kind| kind.category == node.category) {
            Some(kind) => {
                kind.bytes = kind.bytes.saturating_add(node.bytes);
                kind.files += node.files;
            }
            None => kinds.push(KindTotal {
                category: node.category,
                label: node.category.label(),
                bytes: node.bytes,
                files: node.files,
            }),
        }
        let Some(reason) = node.reclaim else {
            continue;
        };
        reclaimable.bytes = reclaimable.bytes.saturating_add(node.bytes);
        match reclaimable
            .reasons
            .iter_mut()
            .find(|total| total.reclaim == reason)
        {
            Some(total) => total.bytes = total.bytes.saturating_add(node.bytes),
            None => reclaimable.reasons.push(ReasonTotal {
                reclaim: reason,
                label: reason.label(),
                bytes: node.bytes,
            }),
        }
    }
    kinds.retain(|kind| kind.bytes > 0 || kind.files > 0);
    kinds.sort_by_key(|kind| std::cmp::Reverse(kind.bytes));
    reclaimable.reasons.retain(|total| total.bytes > 0);
    reclaimable
        .reasons
        .sort_by_key(|total| std::cmp::Reverse(total.bytes));
    (kinds, reclaimable)
}

/// Why a finding is on the list, in the window's words.
fn why(finding: &Finding) -> String {
    let plural = |count: usize| if count == 1 { "" } else { "s" };
    match finding {
        Finding::Reclaimable(reason) => reason.label().to_owned(),
        Finding::Worktrees { count, oldest_days } => format!(
            "{count} worktree{} · oldest {oldest_days} d",
            plural(*count)
        ),
        Finding::StaleExperiments { count } => format!(
            "{count} experiment{} untouched > {STALE_DAYS} d",
            plural(*count)
        ),
    }
}

/// What a row adds about its entry: its kind where that differs from its
/// directory's, and why it can be had back where that starts. Everything
/// beneath inherits both, so repeating them would only be noise.
fn tag(parent: &Entry, child: &Entry) -> String {
    let mut parts: Vec<Cow<'_, str>> = Vec::new();
    if child.category != parent.category && child.category != Category::Other {
        parts.push(Cow::Owned(child.category.label().to_lowercase()));
    }
    if let Some(reason) = child.reclaim
        && parent.reclaim.is_none()
    {
        parts.push(Cow::Borrowed(reason.label()));
    }
    if child.unreadable {
        parts.push(Cow::Borrowed("unreadable"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("  ({})", parts.join(", "))
    }
}

/// A share as a short column: `43%`, or `<1%` for a sliver that is there.
fn percent(part: u64, total: u64) -> String {
    let value = share(part, total);
    if part > 0 && value < 0.5 {
        "<1%".to_owned()
    } else {
        format!("{value:.0}%")
    }
}

/// `text` with its control characters escaped.
fn printable(text: &str) -> Cow<'_, str> {
    if text.chars().any(char::is_control) {
        Cow::Owned(
            text.chars()
                .map(|char| {
                    if char.is_control() {
                        char.escape_default().to_string()
                    } else {
                        char.to_string()
                    }
                })
                .collect(),
        )
    } else {
        Cow::Borrowed(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::classify;
    use crate::scan::{ScanOptions, scan};
    use crate::tree::aggregate;
    use std::fs;
    use tempfile::TempDir;

    const MIB: u64 = 1024 * 1024;
    const DAY: i64 = 86_400;
    const NOW: i64 = 1_800_000_000;

    fn file(name: &str, bytes: u64) -> Node {
        let mut node = Node::entry(name, NodeKind::File, bytes);
        node.modified = NOW - DAY;
        node
    }

    fn dir(name: &str, children: Vec<Node>) -> Node {
        let mut node = Node::directory(name);
        node.children = children;
        node
    }

    fn tree(mut root: Node) -> Node {
        aggregate(&mut root, Metric::Bytes);
        classify(&mut root);
        root
    }

    fn scanned() -> Scanned {
        Scanned {
            root: PathBuf::from("/home/tobi"),
            home: Some(PathBuf::from("/home/tobi")),
            now: NOW,
            ..Scanned::default()
        }
    }

    fn home() -> Node {
        tree(dir(
            "tobi",
            vec![
                dir(
                    "src",
                    vec![
                        file("a", 40),
                        file("b", 30),
                        file("c", 20),
                        file("d", 9),
                        file("e", 1),
                    ],
                ),
                dir(".cache", vec![dir("kache", vec![file("blob", 300)])]),
                dir(
                    "deep",
                    vec![dir("er", vec![dir("est", vec![file("f", 50)])])],
                ),
            ],
        ))
    }

    fn limits(depth: usize, top: usize, min: Threshold) -> Limits {
        Limits {
            depth,
            top,
            min,
            findings: 10,
        }
    }

    fn named<'a>(entry: &'a Entry, name: &str) -> &'a Entry {
        entry
            .children
            .iter()
            .find(|child| child.name == name)
            .unwrap_or_else(|| panic!("no {name} under {}", entry.name))
    }

    #[test]
    fn lists_the_largest_first_and_folds_the_rest() {
        let report =
            Report::new(&home(), scanned(), limits(2, 2, Threshold::Bytes(0)));
        let src = named(&report.tree, "src");
        let names: Vec<&str> =
            src.children.iter().map(|child| &*child.name).collect();
        assert_eq!(names, ["a", "b"]);
        let rest = src.rest.as_ref().expect("a remainder");
        assert_eq!((rest.entries, rest.bytes, rest.files), (3, 30, 3));
        let top: Vec<&str> = report
            .tree
            .children
            .iter()
            .map(|child| &*child.name)
            .collect();
        assert_eq!(top, [".cache", "src"], "largest first");
        assert_eq!(report.tree.rest.as_ref().map(|rest| rest.bytes), Some(50));
    }

    #[test]
    fn depth_stops_the_listing_without_a_remainder() {
        let report =
            Report::new(&home(), scanned(), limits(2, 10, Threshold::Bytes(0)));
        let er = named(named(&report.tree, "deep"), "er");
        assert!(er.children.is_empty());
        assert!(er.rest.is_none(), "the totals say it at the depth limit");
        assert_eq!(er.bytes, 50);
        assert_eq!(er.path, "/home/tobi/deep/er");
    }

    #[test]
    fn a_share_floor_folds_small_entries_into_the_remainder() {
        // 450 bytes in all, so five percent is 22.5: `c` at 20 is out.
        let report = Report::new(
            &home(),
            scanned(),
            limits(2, 10, Threshold::Share(5.0)),
        );
        let src = named(&report.tree, "src");
        let names: Vec<&str> =
            src.children.iter().map(|child| &*child.name).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(src.rest.as_ref().map(|rest| rest.bytes), Some(30));
    }

    #[test]
    fn kinds_count_every_byte_once() {
        let root = home();
        let report = Report::new(&root, scanned(), Limits::default());
        let sum: u64 = report.kinds.iter().map(|kind| kind.bytes).sum();
        assert_eq!(sum, root.bytes);
        assert_eq!(report.kinds[0].category, Category::Cache);
        assert_eq!(report.reclaimable.bytes, 300);
        assert_eq!(report.reclaimable.reasons[0].reclaim, Reclaim::Regenerable);
    }

    #[test]
    fn thresholds_read_sizes_and_shares() {
        assert_eq!(Threshold::parse("1%"), Some(Threshold::Share(1.0)));
        assert_eq!(Threshold::parse(" 0.5 %"), Some(Threshold::Share(0.5)));
        assert_eq!(Threshold::parse("512M"), Some(Threshold::Bytes(512 * MIB)));
        assert_eq!(Threshold::parse("4096"), Some(Threshold::Bytes(4096)));
        for bad in ["", "%", "101%", "-1%", "nan%", "lots", "1Q"] {
            assert_eq!(Threshold::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn text_shortens_home_and_escapes_control_characters() {
        let root = tree(dir(
            "tobi",
            vec![dir("odd\nname", vec![file("x", 10)]), file("y", 5)],
        ));
        let text = Report::new(&root, scanned(), Limits::default()).to_text();
        assert!(text.starts_with("~  15 B · 2 files · 2 dirs\n"), "{text}");
        assert!(text.contains("odd\\nname/"), "{text}");
        assert!(!text.contains("odd\nname"), "{text}");
    }

    #[test]
    fn unreadable_paths_are_counted_and_explained() {
        let text = Report::new(
            &home(),
            Scanned {
                unreadable: 2,
                full_disk_access: Some(false),
                ..scanned()
            },
            Limits::default(),
        )
        .to_text();
        assert!(text.contains("· 2 unreadable"), "{text}");
        assert!(text.contains("2 paths could not be read"), "{text}");
        assert!(text.contains("Full Disk Access"), "{text}");
    }

    /// End to end, over a real tree: a build directory beside its manifest
    /// is a finding with its absolute path, and the JSON says the same.
    #[test]
    fn a_real_scan_reports_build_output_as_worth_a_look() {
        let temp = TempDir::new().expect("tempdir");
        let project = temp.path().join("rust-thing");
        fs::create_dir_all(project.join("target/debug")).expect("mkdir");
        fs::write(project.join("Cargo.toml"), "[package]\n").expect("write");
        // Sparse, and measured by apparent size: past the findings' floor
        // without writing eighty megabytes.
        fs::File::create(project.join("target/debug/app"))
            .and_then(|file| file.set_len(80 * MIB))
            .expect("sparse file");
        fs::write(temp.path().join("notes.txt"), "hello").expect("write");

        let root = temp.path().to_path_buf();
        let options = ScanOptions {
            apparent_size: true,
            ..ScanOptions::default()
        };
        let tree = scan(&root, options).expect("scan");
        let report = Report::new(
            &tree,
            Scanned {
                root: root.clone(),
                apparent_size: true,
                now: NOW,
                ..Scanned::default()
            },
            Limits::default(),
        );

        let target = root.join("rust-thing").join("target");
        let found = report.findings.first().expect("a finding");
        assert_eq!(found.path, target.to_string_lossy());
        assert_eq!(found.finding, "reclaimable");
        assert_eq!(found.reclaim, Some(Reclaim::BuildOutput));
        assert!(found.bytes >= 80 * MIB);

        let json: serde_json::Value =
            serde_json::from_str(&report.to_json().expect("json"))
                .expect("one document");
        assert_eq!(json["measure"], "apparent size");
        assert_eq!(json["findings"][0]["reclaim"], "build_output");
        let first = &json["tree"]["children"][0];
        assert_eq!(first["name"], "rust-thing");
        assert_eq!(first["category"], "other");
        assert_eq!(
            first["children"][0]["path"].as_str(),
            target.to_str(),
            "paths are absolute"
        );
        assert_eq!(json["bytes"], tree.bytes);
    }
}

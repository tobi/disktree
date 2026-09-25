//! Reading a git checkout: what removing it would lose, and whether its
//! work has landed on the base.
//!
//! Only ever read, through [`crate::git`]. Nothing is fetched, so the base
//! is as of the last fetch, which the reading says.

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use crate::classify::Category;
use crate::details::{CheckoutItem, CheckoutKind, exists, normalize};
use crate::git::{self, DIFF_FLAGS, Git, arguments};
use crate::tree::Node;

/// A reading, or why there is none: what git said, so a missing tool or a
/// refused directory is named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reading<T> {
    Done(T),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    Branch(String),
    Detached(String),
    Unborn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Landed {
    Merged,
    /// Every path the branch changed already reads the same on the base.
    OnBase,
    Squashed {
        commit: String,
        subject: String,
    },
    NotLanded,
    /// No squash turned up before the search ran out of time.
    Unfinished,
    NoBase,
    Unknown(String),
}

impl Landed {
    pub const fn is_landed(&self) -> bool {
        matches!(self, Self::Merged | Self::OnBase | Self::Squashed { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Upstream {
    Absent,
    /// Configured, but the remote branch is gone.
    Gone(String),
    Tracking {
        name: String,
        unpushed: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// Porcelain v2's two letters, `.` for unchanged; `??` when untracked.
    pub code: String,
    pub path: String,
}

impl Change {
    /// Content that exists nowhere else; a deleted file is still in the
    /// commit.
    pub fn is_unsaved(&self) -> bool {
        !self.code.chars().all(|letter| matches!(letter, '.' | 'D'))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    pub subject: String,
    pub time: i64,
}

/// A branch checked out here before, still there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Earlier {
    pub branch: String,
    pub landed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkout {
    pub kind: CheckoutKind,
    pub head: Option<Head>,
    /// For a detached head, the branches and remote branches at it.
    pub refs_here: Vec<String>,
    /// A rebase, merge or the like, stopped half way.
    pub operation: Option<&'static str>,
    pub base: Option<String>,
    pub base_fetched: Option<i64>,
    pub landed: Landed,
    pub upstream: Upstream,
    /// The first few, of `change_count`.
    pub changes: Vec<Change>,
    pub change_count: usize,
    pub unsaved_count: usize,
    /// The newest few not on the base, of `commit_count`.
    pub commits: Vec<Commit>,
    pub commit_count: usize,
    /// Commits only a detached head holds.
    pub lost: usize,
    pub earlier: Vec<Earlier>,
    pub stashes: usize,
    pub last_commit: Option<i64>,
    pub worktrees: usize,
    pub prunable: usize,
    /// What git does not keep but the folder holds: nested checkouts, files
    /// beside a wrapped checkout, ignored databases and `.env` files.
    pub leftovers: Vec<String>,
}

impl Checkout {
    const fn new(kind: CheckoutKind) -> Self {
        Self {
            kind,
            head: None,
            refs_here: Vec::new(),
            operation: None,
            base: None,
            base_fetched: None,
            landed: Landed::NoBase,
            upstream: Upstream::Absent,
            changes: Vec::new(),
            change_count: 0,
            unsaved_count: 0,
            commits: Vec::new(),
            commit_count: 0,
            lost: 0,
            earlier: Vec::new(),
            stashes: 0,
            last_commit: None,
            worktrees: 0,
            prunable: 0,
            leftovers: Vec::new(),
        }
    }

    /// Only an unlocked worktree's folder can go without losing anything:
    /// its branches, commits and stashes stay in its repository, so what
    /// counts is what the folder alone holds.
    pub const fn loses_nothing(&self) -> bool {
        matches!(self.kind, CheckoutKind::Worktree { locked: false, .. })
            && self.unsaved_count == 0
            && self.operation.is_none()
            && self.lost == 0
            && self.leftovers.is_empty()
    }

    pub const fn is_landed(&self) -> bool {
        self.landed.is_landed()
    }
}

const SHOWN_CHANGES: usize = 8;
const SHOWN_COMMITS: usize = 5;
const SHOWN_EARLIER: usize = 3;
/// Past this many paths the squash check's pathspec stops paying for
/// itself.
const SQUASH_PATH_LIMIT: usize = 500;

pub fn read_checkout(
    item: &CheckoutItem,
    subtree: Option<&Node>,
) -> Reading<Checkout> {
    let Some(git) = Git::new(&item.at) else {
        return Reading::Failed("git is not installed".into());
    };
    match item.kind {
        CheckoutKind::Orphaned => {
            Reading::Failed("its repository is gone".into())
        }
        CheckoutKind::Bare => Reading::Done(read_bare(&git)),
        CheckoutKind::Repository
        | CheckoutKind::Worktree { .. }
        | CheckoutKind::Submodule => {
            let Some(foreign) = git::foreign_filters(&git) else {
                return Reading::Failed(
                    "its filter settings could not be read".into(),
                );
            };
            let mut before_status = git::filters_off(&foreign);
            before_status.extend(git.watched_status());
            read_working_checkout(&git, item, subtree, &before_status)
        }
    }
}

fn read_bare(git: &Git) -> Checkout {
    let mut checkout = Checkout::new(CheckoutKind::Bare);
    (checkout.worktrees, checkout.prunable) = worktree_counts(git);
    checkout.base = base(git);
    checkout.base_fetched = last_fetch(git.directory());
    checkout
}

/// `before_status` is what `status` alone is run with: the filters the
/// checkout's own config names switched off, and an fsmonitor already
/// watching it.
fn read_working_checkout(
    git: &Git,
    item: &CheckoutItem,
    subtree: Option<&Node>,
    before_status: &[String],
) -> Reading<Checkout> {
    let mut status: Vec<&str> =
        before_status.iter().map(String::as_str).collect();
    // Submodules are other checkouts with configs of their own, never
    // vetted; one inside a worktree is a leftover instead.
    status.extend([
        "status",
        "--porcelain=v2",
        "--branch",
        "-z",
        "--no-ahead-behind",
        "--untracked-files=normal",
        "--ignore-submodules=all",
    ]);
    let status = git.run(&status);
    if !status.ok() {
        return Reading::Failed(if status.error.is_empty() {
            "git status failed".into()
        } else {
            status.error
        });
    }
    let summary = parse_status(&status.stdout, SHOWN_CHANGES);
    let mut checkout = Checkout::new(item.kind.clone());
    checkout.change_count = summary.count;
    checkout.unsaved_count = summary.unsaved;
    checkout.changes = summary.changes;
    checkout.head = match &summary.oid {
        Some(oid) if summary.head.as_deref() == Some("(detached)") => {
            Some(Head::Detached(oid.chars().take(12).collect()))
        }
        Some(_) => summary.head.clone().map(Head::Branch),
        None => Some(Head::Unborn),
    };

    let directories =
        git.run(&["rev-parse", "--absolute-git-dir", "--git-common-dir"]);
    if let [own, common] = directories.lines().as_slice() {
        checkout.operation = operation(Path::new(own));
        checkout.base_fetched = last_fetch(&normalize(&item.at.join(common)));
    }
    if item.kind == CheckoutKind::Repository {
        (checkout.worktrees, checkout.prunable) = worktree_counts(git);
    }
    checkout.leftovers = leftovers(subtree, item, git);
    if summary.oid.is_none() {
        return Reading::Done(checkout);
    }

    checkout.last_commit = git
        .run(&["log", "-1", "--format=%ct", "HEAD"])
        .text()
        .trim()
        .parse()
        .ok();
    checkout.base = base(git);
    if let Some(base) = &checkout.base {
        let range = format!("{base}..HEAD");
        checkout.commit_count = git
            .run(&["rev-list", "--count", &range])
            .count()
            .unwrap_or(0);
        checkout.commits = commits(git, &range);
        checkout.landed = landed(git, "HEAD", base, true);
    }
    if let Some(upstream) = summary.upstream {
        checkout.upstream = if summary.tracks_upstream {
            let range = format!("{upstream}..HEAD");
            Upstream::Tracking {
                unpushed: git
                    .run(&["rev-list", "--count", &range])
                    .count()
                    .unwrap_or(0),
                name: upstream,
            }
        } else {
            Upstream::Gone(upstream)
        };
    }
    match checkout.head.clone() {
        Some(Head::Branch(name)) => {
            checkout.stashes = stashes(git, &name);
            checkout.earlier =
                earlier(git, Some(&name), checkout.base.as_deref());
        }
        Some(Head::Detached(_)) => {
            checkout.refs_here = git
                .run(&[
                    "for-each-ref",
                    "--points-at",
                    "HEAD",
                    "--format=%(refname:short)",
                    "refs/heads",
                    "refs/remotes",
                ])
                .lines();
            if checkout.refs_here.is_empty() && !checkout.is_landed() {
                // Remote branches are left out: a monorepo can hold a
                // hundred thousand, and walking them takes seconds. Leaving
                // them out can only overcount.
                let mut args =
                    vec!["rev-list", "--count", "HEAD", "--not", "--branches"];
                if let Some(base) = &checkout.base {
                    args.push(base);
                }
                checkout.lost = git.run(&args).count().unwrap_or(0);
            }
            checkout.earlier = earlier(git, None, checkout.base.as_deref());
        }
        Some(Head::Unborn) | None => {}
    }
    Reading::Done(checkout)
}

// ── landing ─────────────────────────────────────────────────────────────

/// Merge commits and fast-forwards leave the branch's head on the base; a
/// squash or rebase merge leaves only its content, found by the paths the
/// branch changed or by the patch's id.
fn landed(git: &Git, head: &str, base: &str, squash: bool) -> Landed {
    let ancestor = git.run(&["merge-base", "--is-ancestor", head, base]);
    match ancestor.code {
        Some(0) => return Landed::Merged,
        Some(1) => {}
        _ => return Landed::Unknown(ancestor.error),
    }
    let Some(merge_base) = merge_base(git, head, base) else {
        return Landed::NotLanded;
    };
    let mut names = vec!["diff", "--name-only", "-z"];
    names.extend(DIFF_FLAGS);
    names.extend([merge_base.as_str(), head]);
    let paths = git.run(&names).fields();
    if paths.is_empty() {
        return Landed::OnBase;
    }
    if paths.len() > SQUASH_PATH_LIMIT {
        return Landed::NotLanded;
    }
    let mut same = vec!["diff", "--quiet"];
    same.extend(DIFF_FLAGS);
    same.extend([base, head, "--"]);
    same.extend(paths.iter().map(String::as_str));
    if git.run(&same).ok() {
        return Landed::OnBase;
    }
    if !squash {
        return Landed::NotLanded;
    }

    let mut patch = vec!["diff"];
    patch.extend(DIFF_FLAGS);
    patch.extend([merge_base.as_str(), head]);
    let patch_id = arguments(&["patch-id", "--stable"]);
    let mine = git
        .pipeline(&[arguments(&patch), patch_id.clone()], git::TIMEOUT)
        .lines()
        .first()
        .and_then(|line| line.split(' ').next().map(str::to_owned));
    let Some(mine) = mine else {
        return Landed::NotLanded;
    };
    let since = format!("{merge_base}..{base}");
    let mut history = vec!["log", "-p", "--no-merges", "--format=commit %H"];
    history.extend(DIFF_FLAGS);
    history.extend([since.as_str(), "--"]);
    history.extend(paths.iter().map(String::as_str));
    let theirs = git.pipeline(
        &[arguments(&history), patch_id],
        squash_search_budget(git, &merge_base, base),
    );
    for line in theirs.lines() {
        let mut parts = line.split(' ');
        let (Some(id), Some(commit), None) =
            (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if id != mine {
            continue;
        }
        let found = git.run(&["log", "-1", "--format=%h%x00%s", commit]).text();
        if let Some((commit, subject)) =
            found.trim_end_matches('\n').split_once('\0')
        {
            return Landed::Squashed {
                commit: commit.to_owned(),
                subject: subject.to_owned(),
            };
        }
    }
    if theirs.timed_out {
        Landed::Unfinished
    } else {
        Landed::NotLanded
    }
}

/// Looking for a squash reads the patch of every commit since the branch
/// left its base. In a monorepo that lands work as merge commits that takes
/// seconds and finds nothing, so there an old branch gets a short look:
/// long enough for a small repository, which can squash now and then. The
/// base's own recent history says which kind it is, and reading it never
/// walks the range.
fn squash_search_budget(git: &Git, merge_base: &str, base: &str) -> Duration {
    let recent = git
        .run(&["rev-list", "--first-parent", "--parents", "-n", "200", base])
        .lines();
    let sample = &recent[..recent.len().min(50)];
    let merges = sample
        .iter()
        .filter(|line| line.split(' ').count() > 2)
        .count();
    if merges * 2 < sample.len()
        || recent.iter().any(|line| line.starts_with(merge_base))
    {
        Duration::from_secs(8)
    } else {
        Duration::from_millis(1500)
    }
}

/// A branch that never merged its base back in meets it at one boundary
/// commit, which `rev-list` finds in a tenth of a second; `merge-base`
/// walks for seconds in a monorepo, so it is only asked when there are
/// several.
fn merge_base(git: &Git, head: &str, base: &str) -> Option<String> {
    let range = format!("{base}..{head}");
    let boundaries: Vec<String> = git
        .run(&["rev-list", "--boundary", &range])
        .lines()
        .into_iter()
        .filter_map(|line| line.strip_prefix('-').map(str::to_owned))
        .collect();
    if let [only] = boundaries.as_slice() {
        return Some(only.clone());
    }
    git.run(&["merge-base", head, base])
        .lines()
        .into_iter()
        .next()
}

/// The remote's default branch as last fetched, or a local one.
fn base(git: &Git) -> Option<String> {
    let remote_head =
        git.run(&["symbolic-ref", "-q", "--short", "refs/remotes/origin/HEAD"]);
    if remote_head.ok()
        && let Some(name) = remote_head.lines().into_iter().next()
    {
        return Some(name);
    }
    ["origin/main", "origin/master", "main", "master"]
        .into_iter()
        .find(|candidate| {
            git.run(&[
                "rev-parse",
                "--verify",
                "-q",
                &format!("{candidate}^{{commit}}"),
            ])
            .ok()
        })
        .map(str::to_owned)
}

fn commits(git: &Git, range: &str) -> Vec<Commit> {
    let shown = SHOWN_COMMITS.to_string();
    git.run(&["log", "-z", "-n", &shown, "--format=%h%x1f%ct%x1f%s", range])
        .fields()
        .into_iter()
        .filter_map(|record| {
            let mut parts = record.splitn(3, '\u{1f}');
            let (Some(sha), Some(time), Some(subject)) =
                (parts.next(), parts.next(), parts.next())
            else {
                return None;
            };
            Some(Commit {
                sha: sha.to_owned(),
                subject: subject.to_owned(),
                time: time.parse().unwrap_or(0),
            })
        })
        .collect()
}

/// Stashes made on `branch`, by the reflog messages `git stash` writes.
fn stashes(git: &Git, branch: &str) -> usize {
    let (work, named) = (format!("WIP on {branch}:"), format!("On {branch}:"));
    git.run(&["log", "-g", "--format=%gs", "refs/stash"])
        .lines()
        .iter()
        .filter(|line| line.starts_with(&work) || line.starts_with(&named))
        .count()
}

/// The branches this worktree has had checked out, from its own HEAD's
/// reflog, that still exist. The squash check is left out: they outlive the
/// worktree, so this is for reading, not for deciding.
fn earlier(
    git: &Git,
    current: Option<&str>,
    base: Option<&str>,
) -> Vec<Earlier> {
    const MOVED: &str = "checkout: moving from ";
    let mut names: Vec<String> = Vec::new();
    for line in git
        .run(&["log", "-g", "-n", "200", "--format=%gs", "HEAD"])
        .lines()
    {
        let Some(moves) = line.strip_prefix(MOVED) else {
            continue;
        };
        for name in moves.split(" to ").collect::<Vec<_>>().into_iter().rev() {
            if Some(name) != current
                && !names.iter().any(|known| known == name)
                && !looks_like_sha(name)
            {
                names.push(name.to_owned());
            }
        }
    }
    if names.is_empty() {
        return Vec::new();
    }
    let mut args = vec![
        "for-each-ref".to_owned(),
        "--format=%(refname:short)".to_owned(),
    ];
    args.extend(names.iter().map(|name| format!("refs/heads/{name}")));
    let existing = git.run(&args).lines();
    names
        .into_iter()
        .filter(|name| existing.contains(name))
        .take(SHOWN_EARLIER)
        .map(|branch| Earlier {
            landed: base.is_some_and(|base| {
                landed(git, &branch, base, false).is_landed()
            }),
            branch,
        })
        .collect()
}

fn looks_like_sha(name: &str) -> bool {
    name.len() >= 7 && name.chars().all(|letter| letter.is_ascii_hexdigit())
}

fn operation(git_directory: &Path) -> Option<&'static str> {
    [
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("BISECT_LOG", "bisect"),
    ]
    .into_iter()
    .find(|(marker, _)| exists(&git_directory.join(marker)))
    .map(|(_, name)| name)
}

/// `git fetch` writes `FETCH_HEAD` in the directory of the worktree it ran
/// in, so the newest of them is the last fetch.
fn last_fetch(common: &Path) -> Option<i64> {
    let worktrees = common.join("worktrees");
    let linked = std::fs::read_dir(&worktrees)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("FETCH_HEAD"));
    std::iter::once(common.join("FETCH_HEAD"))
        .chain(linked)
        .filter_map(|path| {
            let modified = std::fs::metadata(path).ok()?.modified().ok()?;
            i64::try_from(modified.duration_since(UNIX_EPOCH).ok()?.as_secs())
                .ok()
        })
        .max()
}

/// Linked worktrees, and how many of them git would prune.
fn worktree_counts(git: &Git) -> (usize, usize) {
    let listed = git.run(&["worktree", "list", "--porcelain", "-z"]);
    let (mut records, mut prunable, mut in_record) = (0_usize, 0_usize, false);
    for field in listed.stdout.split(|&byte| byte == 0) {
        if field.is_empty() {
            in_record = false;
            continue;
        }
        if !in_record {
            records += 1;
            in_record = true;
        }
        if field.starts_with(b"prunable") {
            prunable += 1;
        }
    }
    (records.saturating_sub(1), prunable)
}

// ── what the folder holds that git does not ─────────────────────────────

const KEEPSAKE_LIMIT: usize = 100;

fn is_keepsake(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == ".env"
        || lower.starts_with(".env.")
        || [".sqlite", ".sqlite3", ".db"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
}

/// From the scanned tree: a nested checkout would go with the folder, and
/// an ignored file never reaches a commit. Tracked keepsakes are in the
/// commit and untracked ones are already in the status, so only the ignored
/// ones are asked about. A dot-folder is a tool's (`.nx`, `.turbo`), whose
/// databases are caches; checkouts are still looked for there, since
/// `.claude/worktrees` is one.
fn leftovers(
    node: Option<&Node>,
    item: &CheckoutItem,
    git: &Git,
) -> Vec<String> {
    let Some(node) = node else {
        return Vec::new();
    };
    let mut found: Vec<String> = Vec::new();
    let (node, prefix) = if item.item == item.at {
        (node, String::new())
    } else {
        let inner = item
            .at
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        found.extend(
            node.children
                .iter()
                .map(|child| child.name.to_string())
                .filter(|name| *name != inner && name != ".DS_Store"),
        );
        let Some(checkout) = node.child_named(&inner) else {
            return found;
        };
        (checkout, format!("{inner}/"))
    };
    let mut keepsakes: Vec<String> = Vec::new();
    let mut stack: Vec<(String, &Node, bool)> =
        vec![(String::new(), node, false)];
    while let Some((relative, directory, tooling)) = stack.pop() {
        for child in &directory.children {
            let path = format!("{relative}{}", child.name);
            if child.is_dir() {
                if &*child.name == ".git"
                    || child.reclaim.is_some()
                    || child.category == Category::Cache
                {
                    continue;
                }
                if child.child_named(".git").is_some() {
                    found.push(format!("{prefix}{path}/"));
                } else {
                    stack.push((
                        format!("{path}/"),
                        child,
                        tooling || child.name.starts_with('.'),
                    ));
                }
            } else if !tooling
                && is_keepsake(&child.name)
                && keepsakes.len() < KEEPSAKE_LIMIT
            {
                keepsakes.push(path);
            }
        }
    }
    if !keepsakes.is_empty() {
        // Not `check-ignore`, which Apple's git takes seconds to answer on a
        // monorepo's sparse index, where this takes a blink.
        let mut args = vec![
            "ls-files",
            "-z",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--",
        ];
        args.extend(keepsakes.iter().map(String::as_str));
        found.extend(
            git.run(&args)
                .fields()
                .into_iter()
                .map(|path| format!("{prefix}{path}")),
        );
    }
    found
}

// ── status ──────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct StatusSummary {
    oid: Option<String>,
    head: Option<String>,
    upstream: Option<String>,
    tracks_upstream: bool,
    changes: Vec<Change>,
    count: usize,
    unsaved: usize,
}

/// Porcelain v2 with `-z`: headers, then one record per entry, where a
/// rename's record is followed by one more field, its old path. A huge
/// status (a broken sparse checkout lists every file) is counted without
/// making a string of each entry.
fn parse_status(data: &[u8], keep: usize) -> StatusSummary {
    let mut summary = StatusSummary::default();
    let mut skip_next = false;
    for field in data.split(|&byte| byte == 0) {
        if std::mem::take(&mut skip_next) {
            continue;
        }
        let Some(&kind) = field.first() else {
            continue;
        };
        match kind {
            b'#' => header(&String::from_utf8_lossy(field), &mut summary),
            b'1' | b'2' | b'u' | b'?' => {
                skip_next = kind == b'2';
                let code = if kind == b'?' {
                    "??".to_owned()
                } else {
                    String::from_utf8_lossy(field.get(2..4).unwrap_or_default())
                        .into_owned()
                };
                summary.count += 1;
                let change = Change {
                    code,
                    path: String::new(),
                };
                if change.is_unsaved() {
                    summary.unsaved += 1;
                }
                if summary.changes.len() < keep {
                    // Fields before the path: `1 XY sub mH mI mW hH hI`, a
                    // rename's score too, an unmerged entry's three stages.
                    let spaces = match kind {
                        b'1' => 8,
                        b'2' => 9,
                        b'u' => 10,
                        _ => 1,
                    };
                    summary.changes.push(Change {
                        path: path_after(field, spaces),
                        ..change
                    });
                }
            }
            _ => {}
        }
    }
    summary
}

fn path_after(field: &[u8], spaces: usize) -> String {
    let start = field
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b' ')
        .nth(spaces - 1)
        .map_or(0, |(index, _)| index + 1);
    String::from_utf8_lossy(&field[start..]).into_owned()
}

fn header(line: &str, summary: &mut StatusSummary) {
    let mut parts = line.splitn(3, ' ');
    let (Some(_), Some(key), Some(value)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return;
    };
    match key {
        "branch.oid" => {
            summary.oid = (value != "(initial)").then(|| value.to_owned());
        }
        "branch.head" => summary.head = Some(value.to_owned()),
        "branch.upstream" => summary.upstream = Some(value.to_owned()),
        "branch.ab" => summary.tracks_upstream = true,
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_counted_without_reading_every_path() {
        let fields = [
            "# branch.oid 0123456789abcdef",
            "# branch.head main",
            "# branch.upstream origin/main",
            "1 .D N... 100644 100644 000000 aaa aaa gone.txt",
            "2 R. N... 100644 100644 100644 aaa aaa R100 new name.txt",
            "old name.txt",
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc both.txt",
            "? loose file.txt",
        ];
        let mut data = fields.join("\0").into_bytes();
        data.push(0);
        let summary = parse_status(&data, 3);
        assert_eq!(summary.oid.as_deref(), Some("0123456789abcdef"));
        assert_eq!(summary.upstream.as_deref(), Some("origin/main"));
        assert!(
            !summary.tracks_upstream,
            "no branch.ab line: the upstream is gone"
        );
        assert_eq!(summary.count, 4);
        assert_eq!(summary.unsaved, 3, "the deletion is still in the commit");
        let paths: Vec<&str> = summary
            .changes
            .iter()
            .map(|change| change.path.as_str())
            .collect();
        assert_eq!(paths, ["gone.txt", "new name.txt", "both.txt"]);
        let codes: Vec<&str> = summary
            .changes
            .iter()
            .map(|change| change.code.as_str())
            .collect();
        assert_eq!(codes, [".D", "R.", "UU"]);
    }

    /// Git writes Windows paths as `C:/…`, sometimes in their short form,
    /// which never compare equal to a canonical temporary path.
    #[cfg(unix)]
    mod repositories {
        use std::path::PathBuf;
        use std::process::{Command, Stdio};

        use super::super::*;
        use crate::details::{Detail, checkout_item, checkout_kind, details};
        use crate::scan::{ScanOptions, scan};

        /// A bare origin, a clone of it and worktrees of the clone, made by git
        /// the way a person makes them. Setup ignores the developer's own git
        /// config, so a global `commit.gpgsign` cannot stall it; the reader under
        /// test does not.
        struct Checkouts {
            _temp: tempfile::TempDir,
            root: PathBuf,
            git: PathBuf,
        }

        impl Checkouts {
            fn new() -> Option<Self> {
                let git = git::executable()?.clone();
                let temp = tempfile::tempdir().ok()?;
                let root = std::fs::canonicalize(temp.path()).ok()?;
                let repos = Self {
                    _temp: temp,
                    root,
                    git,
                };
                repos.run(
                    &repos.root,
                    &["init", "-q", "--bare", "-b", "main", "origin.git"],
                );
                repos.run(&repos.root, &["clone", "-q", "origin.git", "repo"]);
                repos.commit(&repos.repo(), "a.txt", "one\n", "Start");
                repos.run(
                    &repos.repo(),
                    &["push", "-q", "-u", "origin", "main"],
                );
                Some(repos)
            }

            fn repo(&self) -> PathBuf {
                self.root.join("repo")
            }

            fn run(&self, directory: &Path, args: &[&str]) -> String {
                let mut command = Command::new(&self.git);
                for (key, _) in std::env::vars_os() {
                    if key.to_string_lossy().starts_with("GIT_") {
                        command.env_remove(key);
                    }
                }
                for role in ["AUTHOR", "COMMITTER"] {
                    command
                        .env(format!("GIT_{role}_NAME"), "Test")
                        .env(format!("GIT_{role}_EMAIL"), "test@example.com")
                        .env(
                            format!("GIT_{role}_DATE"),
                            "2026-01-01T00:00:00Z",
                        );
                }
                let output = command
                    .arg("-C")
                    .arg(directory)
                    .args(["-c", "protocol.file.allow=always"])
                    .args(args)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .stdin(Stdio::null())
                    .stderr(Stdio::null())
                    .output()
                    .expect("git runs");
                String::from_utf8_lossy(&output.stdout).into_owned()
            }

            fn write(directory: &Path, file: &str, text: &str) -> PathBuf {
                let path = directory.join(file);
                std::fs::create_dir_all(path.parent().expect("parent"))
                    .expect("mkdir");
                std::fs::write(&path, text).expect("write");
                path
            }

            fn commit(
                &self,
                directory: &Path,
                file: &str,
                text: &str,
                message: &str,
            ) {
                Self::write(directory, file, text);
                self.run(directory, &["add", "-A"]);
                self.run(directory, &["commit", "-q", "-m", message]);
            }

            fn worktree(&self, relative: &str, branch: &str) -> PathBuf {
                let path = self.root.join(relative);
                let target = path.to_string_lossy().into_owned();
                self.run(
                    &self.repo(),
                    &["worktree", "add", "-q", "-b", branch, &target],
                );
                path
            }

            fn item(path: &Path) -> CheckoutItem {
                let node = scan(path, ScanOptions::default()).expect("scan");
                checkout_item(&node, path).expect("a checkout")
            }

            fn read(path: &Path) -> Checkout {
                let node = scan(path, ScanOptions::default()).expect("scan");
                let item = checkout_item(&node, path).expect("a checkout");
                match read_checkout(&item, Some(&node)) {
                    Reading::Done(checkout) => checkout,
                    Reading::Failed(reason) => {
                        panic!("could not read {}: {reason}", path.display())
                    }
                }
            }
        }

        #[test]
        fn a_merged_clean_worktree_loses_nothing() {
            let Some(repos) = Checkouts::new() else {
                return; // no git on this machine
            };
            let tree = repos.worktree("trees/merged/src", "merged");
            repos.commit(&tree, "b.txt", "two\n", "Add b");
            repos.run(
                &repos.repo(),
                &["merge", "-q", "--no-ff", "merged", "-m", "Merge merged"],
            );
            repos.run(&repos.repo(), &["push", "-q", "origin", "main"]);

            let checkout = Checkouts::read(&repos.root.join("trees/merged"));
            assert_eq!(
                checkout.kind,
                CheckoutKind::Worktree {
                    repository: repos.repo(),
                    locked: false
                }
            );
            assert_eq!(checkout.head, Some(Head::Branch("merged".into())));
            assert_eq!(checkout.base.as_deref(), Some("origin/main"));
            assert_eq!(checkout.landed, Landed::Merged);
            assert_eq!(checkout.commit_count, 0);
            assert!(
                checkout.leftovers.is_empty(),
                "the wrapper holds only the checkout: {:?}",
                checkout.leftovers
            );
            assert!(checkout.loses_nothing() && checkout.is_landed());
        }

        #[test]
        fn a_squash_merge_is_found_by_content_or_by_patch() {
            let Some(repos) = Checkouts::new() else {
                return; // no git on this machine
            };
            let plain = repos.worktree("plain", "plain");
            repos.commit(&plain, "c.txt", "three\n", "Add c");
            repos.commit(&plain, "c.txt", "three\nfour\n", "Extend c");
            repos.run(&repos.repo(), &["merge", "-q", "--squash", "plain"]);
            repos.run(&repos.repo(), &["commit", "-q", "-m", "Land c (#12)"]);
            repos.commit(
                &repos.repo(),
                "other.txt",
                "elsewhere\n",
                "Unrelated",
            );
            repos.run(&repos.repo(), &["push", "-q", "origin", "main"]);
            assert_eq!(
                Checkouts::read(&plain).landed,
                Landed::OnBase,
                "the base moved on only in other files"
            );

            let edited = repos.worktree("edited", "edited");
            repos.commit(&edited, "d.txt", "five\n", "Add d");
            repos.run(&repos.repo(), &["merge", "-q", "--squash", "edited"]);
            repos.run(&repos.repo(), &["commit", "-q", "-m", "Land d (#13)"]);
            repos.commit(&repos.repo(), "d.txt", "five\nsix\n", "Edit d again");
            repos.run(&repos.repo(), &["push", "-q", "origin", "main"]);
            let checkout = Checkouts::read(&edited);
            let Landed::Squashed { subject, .. } = &checkout.landed else {
                panic!(
                    "the base edited d.txt since, so only the patch's id finds \
                     it: {:?}",
                    checkout.landed
                );
            };
            assert_eq!(subject, "Land d (#13)");
            assert_eq!(checkout.commit_count, 1);
        }

        #[test]
        fn uncommitted_work_and_unlanded_commits_are_what_would_be_lost() {
            let Some(repos) = Checkouts::new() else {
                return; // no git on this machine
            };
            let open = repos.worktree("open", "open");
            repos.commit(&open, "app/[id]/page.tsx", "page\n", "Add page");
            Checkouts::write(&open, "a.txt", "changed\n");
            let checkout = Checkouts::read(&open);
            assert_eq!(
                checkout.landed,
                Landed::NotLanded,
                "a pathspec of [id] is read literally, never as a pattern"
            );
            assert_eq!(checkout.commit_count, 1);
            assert_eq!(
                checkout
                    .commits
                    .first()
                    .map(|commit| commit.subject.as_str()),
                Some("Add page")
            );
            assert_eq!(
                checkout.changes,
                [Change {
                    code: ".M".into(),
                    path: "a.txt".into()
                }]
            );
            assert_eq!(checkout.unsaved_count, 1);
            assert!(!checkout.loses_nothing());

            let emptied = repos.worktree("emptied", "emptied");
            std::fs::remove_file(emptied.join("a.txt")).expect("remove");
            let deleted = Checkouts::read(&emptied);
            assert_eq!(deleted.change_count, 1);
            assert_eq!(
                deleted.unsaved_count, 0,
                "a deleted file is still in the commit"
            );
            assert!(deleted.loses_nothing() && deleted.is_landed());
        }

        #[test]
        fn a_rename_is_one_change() {
            let Some(repos) = Checkouts::new() else {
                return; // no git on this machine
            };
            let tree = repos.worktree("moved", "moved");
            repos.run(&tree, &["mv", "a.txt", "b.txt"]);
            let checkout = Checkouts::read(&tree);
            assert_eq!(checkout.change_count, 1);
            assert_eq!(
                checkout
                    .changes
                    .iter()
                    .map(|change| change.path.as_str())
                    .collect::<Vec<_>>(),
                ["b.txt"]
            );
        }

        #[test]
        fn a_detached_head_loses_commits_no_ref_holds() {
            let Some(repos) = Checkouts::new() else {
                return; // no git on this machine
            };
            let review = repos.root.join("review");
            let target = review.to_string_lossy().into_owned();
            repos.run(
                &repos.repo(),
                &["worktree", "add", "-q", "--detach", &target, "main"],
            );
            repos.commit(&review, "e.txt", "e\n", "Loose");
            let loose = Checkouts::read(&review);
            assert!(
                matches!(loose.head, Some(Head::Detached(_))),
                "{:?}",
                loose.head
            );
            assert_eq!(loose.lost, 1);
            assert!(!loose.loses_nothing());

            repos.run(
                &review,
                &["push", "-q", "origin", "HEAD:refs/heads/review"],
            );
            let pushed = Checkouts::read(&review);
            assert_eq!(pushed.refs_here, ["origin/review"]);
            assert_eq!(pushed.lost, 0);
        }

        #[test]
        fn a_lock_or_a_nested_checkout_or_an_ignored_env_keeps_a_worktree() {
            let Some(repos) = Checkouts::new() else {
                return; // no git on this machine
            };
            let locked = repos.worktree("locked", "locked");
            let target = locked.to_string_lossy().into_owned();
            repos.run(&repos.repo(), &["worktree", "lock", &target]);
            let locked_checkout = Checkouts::read(&locked);
            assert_eq!(
                locked_checkout.kind,
                CheckoutKind::Worktree {
                    repository: repos.repo(),
                    locked: true
                }
            );
            assert!(!locked_checkout.loses_nothing());

            let nested = repos.worktree("nested", "nested");
            repos.commit(
                &nested,
                ".gitignore",
                "inner/\n.env\n.tool/\n",
                "Ignore",
            );
            repos.run(&repos.repo(), &["merge", "-q", "--ff-only", "nested"]);
            repos.run(&repos.repo(), &["push", "-q", "origin", "main"]);
            repos.run(&nested, &["init", "-q", "inner"]);
            Checkouts::write(&nested, ".env", "SECRET=1\n");
            Checkouts::write(&nested, ".tool/state.db", "cache\n");
            repos.run(&nested, &["init", "-q", ".tool/checkout"]);
            let checkout = Checkouts::read(&nested);
            assert_eq!(
                checkout.change_count, 0,
                "both are ignored, so status says nothing"
            );
            let mut leftovers = checkout.leftovers.clone();
            leftovers.sort();
            assert_eq!(
                leftovers,
                [".env", ".tool/checkout/", "inner/"],
                "a tool's database is a cache; a checkout inside its folder is not"
            );
            assert!(checkout.is_landed() && !checkout.loses_nothing());
        }

        #[test]
        fn the_repository_itself_never_loses_nothing() {
            let Some(repos) = Checkouts::new() else {
                return; // no git on this machine
            };
            repos.worktree("one", "one");
            let two = repos.worktree("two", "two");
            repos.commit(&two, "t.txt", "t\n", "Two");
            repos.run(&two, &["push", "-q", "-u", "origin", "two"]);
            assert_eq!(
                Checkouts::read(&two).upstream,
                Upstream::Tracking {
                    name: "origin/two".into(),
                    unpushed: 0
                }
            );
            repos.run(
                &repos.repo(),
                &["push", "-q", "origin", "--delete", "two"],
            );
            assert_eq!(
                Checkouts::read(&two).upstream,
                Upstream::Gone("origin/two".into())
            );

            let repository = Checkouts::read(&repos.repo());
            assert_eq!(repository.kind, CheckoutKind::Repository);
            assert_eq!(repository.worktrees, 2);
            assert!(!repository.loses_nothing());
        }

        #[test]
        fn a_folder_of_worktrees_lists_only_the_linked_ones() {
            let Some(repos) = Checkouts::new() else {
                return; // no git on this machine
            };
            for name in ["a", "b"] {
                repos.worktree(&format!("trees/{name}/src"), name);
            }
            repos.run(&repos.root.join("trees"), &["init", "-q", "plain"]);
            let root = scan(&repos.root, ScanOptions::default()).expect("scan");

            let trees = root.child_named("trees").expect("trees");
            let path = repos.root.join("trees");
            let found = details(trees, &path);
            let Some(Detail::Worktrees { repository, items }) = found.first()
            else {
                panic!("trees holds two worktrees and a repository: {found:?}");
            };
            assert_eq!(*repository, repos.repo());
            let mut wrapped: Vec<_> =
                items.iter().map(|item| item.item.clone()).collect();
            wrapped.sort();
            assert_eq!(wrapped, [path.join("a"), path.join("b")]);
            let mut at: Vec<_> =
                items.iter().map(|item| item.at.clone()).collect();
            at.sort();
            assert_eq!(at, [path.join("a/src"), path.join("b/src")]);

            let wrapper = trees.child_named("a").expect("a");
            assert_eq!(
                details(wrapper, &path.join("a")),
                [Detail::Checkout(Checkouts::item(&path.join("a")))]
            );
            assert!(
                details(&root, &repos.root).is_empty(),
                "a folder of repositories is not a roll-up"
            );
            let origin = root.child_named("origin.git").expect("origin");
            assert_eq!(
                checkout_kind(&repos.root.join("origin.git"), Some(origin)),
                Some(CheckoutKind::Bare)
            );
        }

        #[test]
        fn a_folder_of_worktrees_is_worth_a_look_for_the_worktrees_alone() {
            use crate::insights::{Finding, worth_a_look};

            let Some(repos) = Checkouts::new() else {
                return; // no git on this machine
            };
            for name in ["a", "b"] {
                let tree = repos.worktree(&format!("work/{name}/src"), name);
                // Sparse: counted at its length, costing no disk.
                std::fs::File::create(tree.join("big.bin"))
                    .and_then(|file| file.set_len(40 * 1024 * 1024))
                    .expect("sparse file");
            }
            Checkouts::write(&repos.root.join("work"), "notes.txt", "kept\n");
            let options = ScanOptions {
                apparent_size: true,
                ..ScanOptions::default()
            };
            let root = scan(&repos.root, options).expect("scan");
            let found = worth_a_look(
                &root,
                &repos.root,
                chrono::Utc::now().timestamp(),
                10,
            );
            let work = root
                .children
                .iter()
                .position(|child| &*child.name == "work")
                .expect("work");
            let finding = found
                .iter()
                .find(|candidate| candidate.crumbs == [work])
                .unwrap_or_else(|| panic!("work is worth a look: {found:?}"));
            assert!(
                matches!(finding.finding, Finding::Worktrees { count: 2, .. }),
                "{:?}",
                finding.finding
            );
            assert!(
                finding.bytes < root.children[work].bytes,
                "the notes beside them are not counted"
            );
        }

        #[test]
        fn submodules_are_never_rolled_up() {
            let Some(repos) = Checkouts::new() else {
                return; // no git on this machine
            };
            let parent = repos.root.join("parent");
            repos.run(&repos.root, &["clone", "-q", "origin.git", "parent"]);
            let origin =
                repos.root.join("origin.git").to_string_lossy().into_owned();
            for name in ["x", "y"] {
                repos.run(
                    &parent,
                    &[
                        "submodule",
                        "add",
                        "-q",
                        &origin,
                        &format!("libs/{name}"),
                    ],
                );
            }
            let libs = parent.join("libs");
            let node = scan(&libs, ScanOptions::default()).expect("scan");
            assert_eq!(
                checkout_kind(&libs.join("x"), node.child_named("x")),
                Some(CheckoutKind::Submodule)
            );
            assert!(details(&node, &libs).is_empty());
        }

        /// A script that leaves `ran` behind if anything runs it.
        fn trap(directory: &Path, name: &str, body: &str) -> (PathBuf, String) {
            use std::os::unix::fs::PermissionsExt as _;

            let marker = directory.join("ran");
            let script = directory.join(name);
            std::fs::write(
                &script,
                format!("#!/bin/sh\ntouch '{}'\n{body}", marker.display()),
            )
            .expect("write script");
            std::fs::set_permissions(
                &script,
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("chmod");
            (marker, script.to_string_lossy().into_owned())
        }

        #[test]
        fn a_checkout_cannot_make_git_run_its_fsmonitor() {
            let Some(repos) = Checkouts::new() else {
                return; // no git on this machine
            };
            let (marker, hook) = trap(&repos.root, "hook.sh", "");
            repos.run(&repos.repo(), &["config", "core.fsmonitor", &hook]);
            Checkouts::read(&repos.repo());
            assert!(!marker.exists(), "the checkout's fsmonitor ran");
        }

        /// A signed stash and `log.showSignature`: listing stashes the way `git
        /// stash list` does would run the checkout's `gpg.program`.
        #[test]
        fn a_checkout_cannot_make_git_verify_a_signature_with_its_program() {
            use std::io::Write as _;

            let Some(repos) = Checkouts::new() else {
                return; // no git on this machine
            };
            let (marker, gpg) = trap(&repos.root, "gpg.sh", "exit 1\n");
            let tree = repos.run(&repos.repo(), &["mktree"]);
            let commit = format!(
                "tree {}\nauthor t <t@t> 0 +0000\ncommitter t <t@t> 0 +0000\n\
                 gpgsig -----BEGIN PGP SIGNATURE-----\n \n \
                 -----END PGP SIGNATURE-----\n\nstash\n",
                tree.trim()
            );
            let mut child = Command::new(&repos.git)
                .arg("-C")
                .arg(repos.repo())
                .args(["hash-object", "-t", "commit", "-w", "--stdin"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .expect("hash-object");
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(commit.as_bytes())
                .expect("write");
            let id = String::from_utf8_lossy(
                &child.wait_with_output().expect("hash-object").stdout,
            )
            .trim()
            .to_owned();
            repos.run(
                &repos.repo(),
                &[
                    "update-ref",
                    "--create-reflog",
                    "-m",
                    "WIP on main: signed",
                    "refs/stash",
                    &id,
                ],
            );
            repos.run(&repos.repo(), &["config", "log.showSignature", "true"]);
            repos.run(&repos.repo(), &["config", "gpg.program", &gpg]);

            let checkout = Checkouts::read(&repos.repo());
            assert!(!marker.exists(), "the checkout's gpg.program ran");
            assert_eq!(checkout.stashes, 1, "the stash is still counted");
        }

        #[test]
        fn a_checkouts_own_filter_is_never_run() {
            let Some(repos) = Checkouts::new() else {
                return; // no git on this machine
            };
            // A clean filter reads the file on stdin and writes it back.
            let (marker, filter) = trap(&repos.root, "filter.sh", "cat\n");
            repos.commit(
                &repos.repo(),
                ".gitattributes",
                "*.txt filter=evil\n",
                "Filter",
            );
            std::fs::remove_file(&marker).ok();
            repos.run(&repos.repo(), &["config", "filter.evil.clean", &filter]);
            // Same content, new timestamp: git must re-read the file, through
            // the filter, to know whether it changed.
            Checkouts::write(&repos.repo(), "a.txt", "one\n");

            let checkout = Checkouts::read(&repos.repo());
            assert!(!marker.exists(), "the checkout's filter ran");
            assert_eq!(
                checkout.change_count, 0,
                "read with the filter off, the file is as committed"
            );
        }
    }
}

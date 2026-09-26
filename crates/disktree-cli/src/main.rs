//! disktree without a window, for scripts and coding agents.
//!
//! The window answers "what is filling this disk" for a person with a mouse.
//! An agent asked the same question otherwise reaches for `find` and `du`,
//! which count clones and hardlinks twice, skip nothing on other volumes and
//! know nothing about what a directory is. This prints what the window
//! would show — the scan totals and the "Worth a look" list — as JSON, and
//! removes paths only through the same guards the review screen uses, and
//! only to the trash.
//!
//! ```text
//! disktree-cli scan [PATH] [--store DIR]... [--limit N]
//! disktree-cli check --root ROOT PATH...
//! disktree-cli trash --root ROOT PATH...
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use disktree_core::classify::classify;
use disktree_core::git::{self, GitState};
use disktree_core::insights::{Candidate, Finding, weight_files, worth_a_look};
use disktree_core::removal::{self, Plan, Target};
use disktree_core::scan::{ScanOptions, scan};
use disktree_core::space::space_info;
use disktree_core::tree::{Node, path_of};
use serde_json::{Value, json};

/// Seconds in a day.
const DAY: i64 = 86_400;

/// Enough for a sweep of a home directory; the window shows fewer.
const DEFAULT_LIMIT: usize = 100;

const USAGE: &str = "\
disktree-cli: disktree without a window. Every command prints JSON.

usage:
  disktree-cli scan [PATH] [--store DIR]... [--limit N]
      Scan PATH (default: the home directory) and list what is worth a
      look, largest first. A --store is a directory kept on purpose, such
      as a model store: findings inside it are left out, and a weight file
      found elsewhere with the same name and size as one in a store is
      reported as a copy of it.
  disktree-cli check --root ROOT PATH...
      Say which PATHs the removal guards accept, without touching them.
  disktree-cli trash --root ROOT PATH...
      Move the accepted PATHs to the system trash. Never deletes outright.
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(output) => println!("{output:#}"),
        Err(error) => {
            eprintln!("disktree-cli: {error:#}");
            std::process::exit(2);
        }
    }
}

fn run(args: &[String]) -> Result<Value> {
    let Some((command, rest)) = args.split_first() else {
        eprint!("{USAGE}");
        std::process::exit(2);
    };
    match command.as_str() {
        "scan" => scan_command(rest),
        "check" => removal_command(rest, false),
        "trash" => removal_command(rest, true),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            std::process::exit(0);
        }
        other => bail!("unknown command `{other}`; see --help"),
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path)
        .with_context(|| format!("cannot resolve {}", path.display()))
}

fn scan_command(args: &[String]) -> Result<Value> {
    let mut root = None;
    let mut stores = Vec::new();
    let mut limit = DEFAULT_LIMIT;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--store" => {
                let store = args.next().context("--store needs a directory")?;
                stores.push(absolute(Path::new(store))?);
            }
            "--limit" => {
                let value = args.next().context("--limit needs a number")?;
                limit = value.parse().context("--limit needs a number")?;
            }
            flag if flag.starts_with('-') => bail!("unknown option `{flag}`"),
            path => root = Some(absolute(Path::new(path))?),
        }
    }
    let root = match root {
        Some(root) => root,
        None => std::env::home_dir().context("no home directory")?,
    };

    let started = Instant::now();
    let mut tree = scan(&root, ScanOptions::default())
        .with_context(|| format!("cannot scan {}", root.display()))?;
    classify(&mut tree);
    let now = now();
    let store_files = index_stores(&stores)?;
    // Asked for everything, then filtered: a finding inside a store must not
    // push a real one off the end of the list.
    let findings: Vec<Value> = worth_a_look(&tree, now, usize::MAX)
        .iter()
        .filter_map(|candidate| {
            let path = path_of(&root, &tree, &candidate.crumbs);
            let inside_store =
                stores.iter().any(|store| path.starts_with(store));
            (!inside_store).then(|| {
                let node = tree.resolve(&candidate.crumbs)?;
                Some(finding_json(candidate, node, &path, now, &store_files))
            })?
        })
        .take(limit)
        .collect();

    Ok(json!({
        "root": root,
        "scanned_at": now,
        "elapsed_ms": started.elapsed().as_millis() as u64,
        "totals": {
            "bytes": tree.bytes,
            "files": tree.files,
            "dirs": tree.dirs,
            "unreadable_dirs": unreadable(&tree),
        },
        "space": space_json(&root),
        "stores": stores,
        "findings": findings,
    }))
}

/// Every file in the stores, by name and size: a weight file elsewhere with
/// the same pair is almost certainly the same download.
fn index_stores(
    stores: &[PathBuf],
) -> Result<HashMap<(Box<str>, u64), PathBuf>> {
    let mut index = HashMap::new();
    for store in stores {
        let tree = scan(store, ScanOptions::default()).with_context(|| {
            format!("cannot scan store {}", store.display())
        })?;
        let mut crumbs = Vec::new();
        collect_files(&tree, store, &tree, &mut crumbs, &mut index);
    }
    Ok(index)
}

fn collect_files(
    root: &Node,
    root_path: &Path,
    node: &Node,
    crumbs: &mut Vec<usize>,
    index: &mut HashMap<(Box<str>, u64), PathBuf>,
) {
    for (position, child) in node.children.iter().enumerate() {
        crumbs.push(position);
        if child.is_dir() {
            collect_files(root, root_path, child, crumbs, index);
        } else {
            index
                .entry((child.name.clone(), child.bytes))
                .or_insert_with(|| path_of(root_path, root, crumbs));
        }
        crumbs.pop();
    }
}

fn finding_json(
    candidate: &Candidate,
    node: &Node,
    path: &Path,
    now: i64,
    store_files: &HashMap<(Box<str>, u64), PathBuf>,
) -> Value {
    let modified_days =
        (node.modified > 0).then(|| (now - node.modified) / DAY);
    let mut finding = json!({
        "path": path,
        "bytes": candidate.bytes,
        "modified_days": modified_days,
    });
    let fields = match &candidate.finding {
        Finding::Reclaimable(reason) => json!({
            "kind": "reclaimable",
            "reason": reason.label(),
            // The one kind whose space is known to come back: a tool or a
            // build writes it again.
            "tier": "regenerable",
        }),
        Finding::Worktrees { count, oldest_days } => json!({
            "kind": "worktrees",
            "count": count,
            "oldest_days": oldest_days,
            "tier": "judge",
            "checkouts": checkouts(node, path),
        }),
        Finding::StaleExperiments { count } => json!({
            "kind": "stale_experiments",
            "count": count,
            "tier": "judge",
        }),
        Finding::Weights { files } => {
            let copies: Vec<Value> = weight_files(node)
                .filter_map(|file| {
                    let copy = store_files.get(&(file.name.clone(), file.bytes))?;
                    Some(json!({ "file": path.join(&*file.name), "store_copy": copy }))
                })
                .collect();
            json!({
                "kind": "weights",
                "files": files,
                "copies_in_store": copies.len(),
                "copies": copies,
                "tier": "judge",
            })
        }
        Finding::StaleArchive { days } => json!({
            "kind": "stale_archive",
            "days": days,
            "tier": "judge",
        }),
    };
    if let (Value::Object(finding), Value::Object(fields)) =
        (&mut finding, fields)
    {
        finding.extend(fields);
    }
    finding
}

/// The checkouts directly in a worktrees directory, with what deleting
/// each would lose.
fn checkouts(node: &Node, path: &Path) -> Vec<Value> {
    node.children
        .iter()
        .filter(|child| child.is_dir())
        .map(|child| {
            let path = path.join(&*child.name);
            let state = git::state(&path);
            json!({
                "path": path,
                "bytes": child.bytes,
                "git": state.as_ref().map(git_json),
            })
        })
        .collect()
}

fn git_json(state: &GitState) -> Value {
    json!({
        "changed": state.changed,
        "stashes": state.stashes,
        "unpushed": state.unpushed,
        "clean": state.is_clean(),
        "summary": state.summary(),
    })
}

fn unreadable(node: &Node) -> u64 {
    u64::from(node.read_error)
        + node.children.iter().map(unreadable).sum::<u64>()
}

fn space_json(path: &Path) -> Value {
    space_info(path).map_or(
        Value::Null,
        |space| json!({ "total": space.total, "available": space.available }),
    )
}

fn removal_command(args: &[String], act: bool) -> Result<Value> {
    let mut root = None;
    let mut paths = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => {
                let value = args.next().context("--root needs a directory")?;
                root = Some(absolute(Path::new(value))?);
            }
            flag if flag.starts_with('-') => bail!("unknown option `{flag}`"),
            path => paths.push(absolute(Path::new(path))?),
        }
    }
    // Required, not defaulted: the root is the consent boundary, as the
    // scanned root is in the window.
    let root = root.context("--root is required: the tree you looked at")?;
    if paths.is_empty() {
        bail!("no paths given");
    }
    let targets = paths
        .iter()
        .map(|path| target(path))
        .collect::<Result<Vec<_>>>()?;
    let plan = removal::plan(&targets, &root);
    let before = space_info(&root).ok();
    let mut results = Vec::new();
    if act {
        let backend = removal::detect_trash_backend();
        for target in &plan.targets {
            let outcome = removal::move_to_trash(&target.path, backend);
            results.push(json!({
                "path": target.path,
                "bytes": target.bytes,
                "trashed": outcome.is_ok(),
                "error": outcome.err().map(|error| error.to_string()),
            }));
        }
    }
    let after = space_info(&root).ok();
    Ok(json!({
        "root": plan.root,
        "mode": if act { "trash" } else { "check" },
        "plan": plan_json(&plan),
        "results": results,
        "available_before": before.map(|space| space.available),
        "available_after": after.map(|space| space.available),
    }))
}

fn target(path: &Path) -> Result<Target> {
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let is_dir = meta.is_dir();
    let bytes = if is_dir {
        scan(path, ScanOptions::default())
            .with_context(|| format!("cannot scan {}", path.display()))?
            .bytes
    } else {
        allocated(&meta)
    };
    let hidden = path
        .file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with('.'));
    Ok(Target {
        path: path.to_path_buf(),
        bytes,
        is_dir,
        hidden,
    })
}

/// What deleting a file gives back, the way the scan counts it.
#[cfg(unix)]
fn allocated(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt as _;
    meta.blocks() * 512
}

#[cfg(not(unix))]
fn allocated(meta: &std::fs::Metadata) -> u64 {
    meta.len()
}

fn plan_json(plan: &Plan) -> Value {
    let target =
        |target: &Target| json!({ "path": target.path, "bytes": target.bytes });
    json!({
        "accepted": plan.targets.iter().map(target).collect::<Vec<_>>(),
        "covered": plan.covered.iter().map(target).collect::<Vec<_>>(),
        "blocked": plan
            .blocked
            .iter()
            .map(|blocked| json!({ "path": blocked.path, "reason": blocked.reason }))
            .collect::<Vec<_>>(),
        "bytes": plan.bytes(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn scan_finds_weights_and_matches_the_store() {
        let home = tempfile::tempdir().expect("tempdir");
        let project = home.path().join("project/models");
        let store = home.path().join("Models/llama");
        std::fs::create_dir_all(&project).expect("mkdir");
        std::fs::create_dir_all(&store).expect("mkdir");
        let weights = vec![7_u8; 70 * 1024 * 1024];
        std::fs::write(project.join("model.gguf"), &weights).expect("write");
        std::fs::write(store.join("model.gguf"), &weights).expect("write");

        let output = run(&strings(&[
            "scan",
            home.path().to_str().expect("utf-8"),
            "--store",
            home.path().join("Models").to_str().expect("utf-8"),
        ]))
        .expect("scan");
        let findings = output["findings"].as_array().expect("findings");
        assert_eq!(findings.len(), 1, "the store's own copy is left out");
        assert_eq!(findings[0]["kind"], "weights");
        assert_eq!(findings[0]["copies_in_store"], 1);
        assert_eq!(findings[0]["tier"], "judge");
    }

    #[test]
    fn check_refuses_the_root_and_paths_outside_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inside = dir.path().join("build");
        std::fs::create_dir(&inside).expect("mkdir");
        let root = dir.path().to_str().expect("utf-8");
        let output = run(&strings(&[
            "check",
            "--root",
            root,
            inside.to_str().expect("utf-8"),
            root,
        ]))
        .expect("check");
        assert_eq!(
            output["plan"]["accepted"].as_array().map(Vec::len),
            Some(1)
        );
        assert_eq!(output["plan"]["blocked"].as_array().map(Vec::len), Some(1));
        assert!(inside.exists(), "check never touches anything");
    }

    #[test]
    fn removal_needs_a_root() {
        assert!(run(&strings(&["check", "/tmp/x"])).is_err());
    }
}

//! A scan, and the JSON report made from it.
//!
//! The walk is the core scanner's; this module only reads the finished
//! tree, once, and does every sum, share and age the report states, so a
//! reader never has to add sizes or subtract dates. Lists are kept to
//! their top entries as the tree is read, so a disk of millions of files
//! costs a few hundred paths, not millions.
//!
//! The breakdowns are built to be added up: each divides the root's bytes
//! into parts that do not overlap and sum to the total exactly.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use disktree_core::classify::{
    Category, Reclaim, category_of_name, is_git_store,
};
use disktree_core::exclude::Exclude;
use disktree_core::scan::{ScanHandle, ScanSnapshot};
use disktree_core::size::{human_bytes, human_count};
use disktree_core::space::{self, SpaceInfo};
use disktree_core::tree::{Node, NodeKind};
use serde_json::{Map, Value, json};

use crate::query::{
    Filters, Query, ReclaimFilter, ScanSettings, category_slug, reclaim_slug,
};
use crate::{SCHEMA_VERSION, VERSION};

/// Unreadable paths quoted in a report; the count covers the rest.
const ERROR_EXAMPLES: usize = 10;

/// How often a running scan reports progress.
const PROGRESS_EVERY: Duration = Duration::from_millis(500);

const DAY: i64 = 86_400;

/// The age buckets of `breakdown.by_age`: label and upper bound in days.
const AGES: [(&str, i64); 4] = [
    ("under_30_days", 30),
    ("30_days_to_6_months", 182),
    ("6_months_to_1_year", 365),
    ("over_1_year", i64::MAX),
];

/// Every category, in the index order the walk tallies them by.
const CATEGORIES: [Category; 9] = [
    Category::Code,
    Category::AgentScratch,
    Category::Toolchain,
    Category::Synced,
    Category::Git,
    Category::Media,
    Category::Documents,
    Category::Cache,
    Category::Other,
];

/// Every reclaim reason, in the index order the walk tallies them by.
const RECLAIMS: [Reclaim; 9] = [
    Reclaim::Regenerable,
    Reclaim::SyncHistory,
    Reclaim::PackageStore,
    Reclaim::BuildOutput,
    Reclaim::Reinstallable,
    Reclaim::SandboxLayers,
    Reclaim::Snapshots,
    Reclaim::Trash,
    Reclaim::Temporary,
];

/// A finished scan, ready to be reported on any number of times.
#[derive(Debug)]
pub struct Scanned {
    pub root: PathBuf,
    /// Shared, so the window can report on the tree it already holds.
    pub tree: Arc<Node>,
    /// The SSH host the tree was scanned on, or `None` for this computer.
    /// A remote root says nothing about this computer's disks.
    pub host: Option<String>,
    pub settings: ScanSettings,
    /// Unix seconds when the scan finished.
    pub scanned_at: i64,
    pub elapsed: Duration,
    pub errors: u64,
    pub error_examples: Vec<String>,
}

/// Scan `root`, calling `progress` about twice a second while the walk
/// runs and `cancelled` as often, to stop it early.
pub fn scan(
    root: &Path,
    settings: &ScanSettings,
    progress: &mut dyn FnMut(&ScanSnapshot),
    cancelled: &mut dyn FnMut() -> bool,
) -> io::Result<Scanned> {
    let options = settings.options().map_err(io::Error::other)?;
    let started = Instant::now();
    let handle = ScanHandle::spawn(root.to_path_buf(), options);
    let mut last_report = Instant::now();
    let tree = loop {
        // Asked before the result is taken, so a cancellation already
        // waiting is seen even by a scan that finishes at once.
        if cancelled() {
            handle.cancel();
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "the scan was cancelled",
            ));
        }
        if let Some(outcome) = handle.poll() {
            break outcome?;
        }
        if last_report.elapsed() >= PROGRESS_EVERY {
            progress(&handle.progress.snapshot());
            last_report = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let snapshot = handle.progress.snapshot();
    Ok(Scanned {
        root: root.to_path_buf(),
        tree: Arc::new(tree),
        host: None,
        settings: settings.clone(),
        scanned_at: chrono::Utc::now().timestamp(),
        elapsed: started.elapsed(),
        errors: snapshot.errors,
        error_examples: snapshot
            .messages
            .into_iter()
            .take(ERROR_EXAMPLES)
            .collect(),
    })
}

/// A scan without progress or cancellation, for the command line.
pub fn scan_quietly(
    root: &Path,
    settings: &ScanSettings,
) -> io::Result<Scanned> {
    scan(root, settings, &mut |_| {}, &mut || false)
}

/// The report for `query` over `scanned`, as of `now` (Unix seconds).
pub fn report(
    scanned: &Scanned,
    query: &Query,
    now: i64,
) -> Result<Value, String> {
    let names = Exclude::new(&query.filters.names)?;
    let tree = &scanned.tree;
    let total = tree.bytes;
    let mut walk = Walk::new(query, names, now, total);
    let mut path = scanned.root.clone();
    walk.visit(tree, &mut path, 1, Category::Other, true);

    let sections = query.sections;
    // This computer's free space, measured here: never for a root on a
    // server, whose path may not exist here or may name something else.
    let volume = (sections.volume() && scanned.host.is_none())
        .then(|| space::space_info(&scanned.root).ok())
        .flatten();

    let mut report = Map::new();
    let mut put = |key: &str, value: Value| {
        report.insert(key.to_owned(), value);
    };
    put("schema_version", SCHEMA_VERSION.into());
    put(
        "generator",
        json!({ "name": "disktree", "version": VERSION }),
    );
    if let Some(host) = &scanned.host {
        put("host", host.clone().into());
    }
    put("root", path_text(&scanned.root).into());
    put("scanned_at", time_text(scanned.scanned_at).into());
    put("scan_seconds", round1(scanned.elapsed.as_secs_f64()).into());
    put("options", options_json(&scanned.settings, query));
    put("filters", filters_json(&query.filters, now));
    put(
        "total",
        json!({
            "bytes": total,
            "size": human_bytes(total),
            "files": tree.files,
            "directories": tree.dirs.saturating_sub(1),
        }),
    );
    put(
        "errors",
        json!({
            "unreadable_paths": scanned.errors,
            "examples": scanned.error_examples,
            "note": (scanned.errors > 0).then_some(
                "These paths could not be read, so their contents are not \
                 counted anywhere in this report."
            ),
        }),
    );
    if sections.volume() {
        put(
            "volume",
            volume.map_or(Value::Null, |info| volume_json(info, total)),
        );
    }
    if sections.overview {
        put("breakdown", walk.breakdown_json(tree, &scanned.root, now));
    }
    if sections.reclaimable() {
        // Only on-disk bytes from this volume come back to it: apparent
        // sizes overstate sparse files, and other volumes are reached by
        // crossing filesystems or following links.
        let settings = &scanned.settings;
        let after = volume.filter(|_| {
            settings.one_filesystem
                && !settings.apparent_size
                && !settings.follow_links
        });
        put("reclaimable", walk.reclaimable_json(after, now));
    }
    if sections.dirs {
        let dirs = std::mem::replace(&mut walk.dirs, Top::new(0));
        put("largest_dirs", dirs_json(&dirs.into_sorted(), total, now));
    }
    if sections.files {
        let files = std::mem::replace(&mut walk.files, Top::new(0));
        let entries = files
            .into_sorted()
            .iter()
            .map(|hit| hit.json(total, now))
            .collect();
        put("largest_files", Value::Array(entries));
    }
    put("matched", walk.matched_json());
    Ok(Value::Object(report))
}

/// A report as the JSON text `disktree scan --json` prints: indented, with
/// a final newline. The window's export writes exactly this.
pub fn json_text(report: &Value) -> String {
    let mut text =
        serde_json::to_string_pretty(report).unwrap_or_else(|_| "{}".into());
    text.push('\n');
    text
}

/// The volume a path is on, without scanning anything.
pub fn summary(path: &Path) -> Result<Value, String> {
    let info = space::space_info(path).map_err(|error| {
        format!("cannot measure {}: {error}", path.display())
    })?;
    Ok(json!({
        "schema_version": SCHEMA_VERSION,
        "generator": { "name": "disktree", "version": VERSION },
        "path": path_text(path),
        "volume_root": space::volume_root_for(path).as_deref().map(path_text),
        "device": space::device_for(path),
        "capacity": amount(info.total),
        "used": amount(info.used()),
        "free": amount(info.free),
        "available": amount(info.available),
        "used_percent": percent(info.used(), info.total),
        "available_percent": percent(info.available, info.total),
    }))
}

/// A report as text for a person at a terminal.
pub fn text(report: &Value) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let total = &report["total"];
    let _ = writeln!(
        out,
        "{}  {} {} · {} files · {} directories · {}s",
        report["root"].as_str().unwrap_or_default(),
        total["size"].as_str().unwrap_or_default(),
        if report["options"]["size_mode"] == "apparent" {
            "apparent"
        } else {
            "on disk"
        },
        human_count(total["files"].as_u64().unwrap_or_default()),
        human_count(total["directories"].as_u64().unwrap_or_default()),
        report["scan_seconds"],
    );
    let volume = &report["volume"];
    if volume.is_object() {
        let _ = writeln!(
            out,
            "volume: {} available of {} ({}% used); this tree is {}% of \
             what is used",
            volume["available"]["size"].as_str().unwrap_or_default(),
            volume["capacity"]["size"].as_str().unwrap_or_default(),
            volume["used_percent"],
            volume["root_percent_of_used"],
        );
    }
    let row = |out: &mut String, entry: &Value, label: &str| {
        let _ = writeln!(
            out,
            "  {:>10} {:>5}%  {}",
            entry["size"].as_str().unwrap_or_default(),
            entry["percent"],
            label,
        );
    };
    if let Some(categories) = report["breakdown"]["by_category"].as_array() {
        let _ = writeln!(out, "\nBy kind");
        for entry in categories {
            row(
                &mut out,
                entry,
                entry["category"].as_str().unwrap_or_default(),
            );
        }
    }
    let reclaimable = &report["reclaimable"];
    if reclaimable.is_object() {
        let _ = writeln!(
            out,
            "\nReclaimable: {} ({}% of this tree)",
            reclaimable["size"].as_str().unwrap_or_default(),
            reclaimable["percent"],
        );
        for entry in reclaimable["by_reason"].as_array().into_iter().flatten() {
            let label = format!(
                "{} ({} places)",
                entry["reason"].as_str().unwrap_or_default(),
                entry["places"],
            );
            row(&mut out, entry, &label);
        }
    }
    for (key, title) in [
        ("largest_dirs", "Largest directories"),
        ("largest_files", "Largest files"),
    ] {
        let Some(entries) = report[key].as_array() else {
            continue;
        };
        let _ = writeln!(out, "\n{title}");
        if entries.is_empty() {
            let _ = writeln!(out, "  (none match)");
        }
        for entry in entries {
            let age = entry["age_days"]
                .as_i64()
                .map_or_else(String::new, |days| format!("{days}d"));
            let _ = writeln!(
                out,
                "  {:>10} {:>5}%  {age:>6}  {}",
                entry["size"].as_str().unwrap_or_default(),
                entry["percent"],
                entry["path"].as_str().unwrap_or_default(),
            );
        }
    }
    let errors = report["errors"]["unreadable_paths"]
        .as_u64()
        .unwrap_or_default();
    if errors > 0 {
        let _ = writeln!(
            out,
            "\n{errors} paths could not be read; their contents are not \
             counted."
        );
    }
    out
}

/// What each reclaim reason means for whoever acts on it, said once per
/// report rather than once per entry.
const fn reclaim_description(reclaim: Reclaim) -> &'static str {
    match reclaim {
        Reclaim::Regenerable => {
            "Caches: whatever wrote them writes them again when needed."
        }
        Reclaim::SyncHistory => {
            "A sync client's old versions of files; current files are kept \
             elsewhere."
        }
        Reclaim::PackageStore => {
            "A package manager's shared store; packages download again when \
             a project needs them."
        }
        Reclaim::BuildOutput => {
            "Compiler or bundler output; the next build recreates it, more \
             slowly."
        }
        Reclaim::Reinstallable => {
            "Installed dependencies beside their manifest; reinstall with the \
             project's package manager."
        }
        Reclaim::SandboxLayers => {
            "Container or sandbox image layers; pulled again when needed."
        }
        Reclaim::Snapshots => {
            "Sandbox or VM snapshots; check none is still wanted first."
        }
        Reclaim::Trash => "Files already deleted, still on disk until emptied.",
        Reclaim::Temporary => "Temporary files and logs.",
    }
}

/// `part` as a percentage of `whole`, one decimal; `0` of nothing.
fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        round1(part as f64 * 100.0 / whole as f64)
    }
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn amount(bytes: u64) -> Value {
    json!({ "bytes": bytes, "size": human_bytes(bytes) })
}

/// Whole days since `modified`; `None` when unknown. Never negative: a
/// clock that has gone back makes a file new, not from the future.
fn age_days(modified: i64, now: i64) -> Option<i64> {
    (modified != 0).then(|| (now - modified).max(0) / DAY)
}

/// Unix seconds as RFC 3339 UTC; `None` for `0`, which the tree uses for
/// unknown.
fn time_text(seconds: i64) -> Option<String> {
    (seconds != 0)
        .then(|| chrono::DateTime::from_timestamp(seconds, 0))
        .flatten()
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn options_json(settings: &ScanSettings, query: &Query) -> Value {
    json!({
        "size_mode": settings.size_mode(),
        "one_filesystem": settings.one_filesystem,
        "include_hidden": settings.include_hidden,
        "follow_links": settings.follow_links,
        "exclude": settings.exclude,
        "top": query.top,
        "depth": query.depth,
    })
}

fn filters_json(filters: &Filters, now: i64) -> Value {
    json!({
        "min_size_bytes": filters.min_size,
        "extensions": filters.extensions,
        "older_than": filters
            .older_than
            .and_then(|older| time_text(older.cutoff(now))),
        "name": filters.names,
        "category": filters.category.map(category_slug),
        "reclaim": filters.reclaim.map(|reclaim| match reclaim {
            ReclaimFilter::Any => "any",
            ReclaimFilter::One(reason) => reclaim_slug(reason),
        }),
    })
}

fn volume_json(info: SpaceInfo, total: u64) -> Value {
    json!({
        "capacity": amount(info.total),
        "used": amount(info.used()),
        "available": amount(info.available),
        "used_percent": percent(info.used(), info.total),
        "available_percent": percent(info.available, info.total),
        "root_percent_of_used": percent(total, info.used()),
    })
}

/// `largest_dirs`, each marked with the nearest listed directory holding
/// it, so overlapping entries are never added together by mistake.
fn dirs_json(hits: &[Hit], total: u64, now: i64) -> Value {
    let listed: HashSet<&Path> =
        hits.iter().map(|hit| hit.path.as_path()).collect();
    let entries = hits
        .iter()
        .map(|hit| {
            let mut entry = hit.json(total, now);
            let nested_in = hit
                .path
                .ancestors()
                .skip(1)
                .find(|ancestor| listed.contains(ancestor))
                .map(path_text);
            if let Some(object) = entry.as_object_mut() {
                object.insert("nested_in".into(), nested_in.into());
            }
            entry
        })
        .collect();
    Value::Array(entries)
}

/// A byte and item tally.
#[derive(Clone, Copy, Debug, Default)]
struct Tally {
    bytes: u64,
    count: u64,
}

impl Tally {
    const fn add(&mut self, bytes: u64, count: u64) {
        self.bytes += bytes;
        self.count += count;
    }
}

/// One listed entry.
#[derive(Debug)]
struct Hit {
    bytes: u64,
    path: PathBuf,
    kind: NodeKind,
    modified: i64,
    depth: usize,
    files: u64,
    unreadable: bool,
    category: Category,
    reclaim: Option<Reclaim>,
}

impl Hit {
    fn new(node: &Node, path: &Path, depth: usize, category: Category) -> Self {
        Self {
            bytes: node.bytes,
            path: path.to_path_buf(),
            kind: node.kind,
            modified: node.modified,
            depth,
            files: node.files,
            unreadable: node.read_error,
            category,
            reclaim: node.reclaim,
        }
    }

    fn json(&self, total: u64, now: i64) -> Value {
        let mut entry = json!({
            "path": path_text(&self.path),
            "name": self
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            "type": if self.kind.is_dir() { "directory" } else { "file" },
            "bytes": self.bytes,
            "size": human_bytes(self.bytes),
            "percent": percent(self.bytes, total),
            "modified": time_text(self.modified),
            "age_days": age_days(self.modified, now),
            "depth": self.depth,
            "category": category_slug(self.category),
            "reclaim": self.reclaim.map(reclaim_slug),
        });
        if self.kind.is_dir()
            && let Some(object) = entry.as_object_mut()
        {
            object.insert("files".into(), self.files.into());
            object.insert("unreadable".into(), self.unreadable.into());
        }
        entry
    }

    /// Larger first; equal sizes by path, so output is stable.
    fn rank(&self, other: &Self) -> Ordering {
        other
            .bytes
            .cmp(&self.bytes)
            .then_with(|| self.path.cmp(&other.path))
    }
}

impl PartialEq for Hit {
    fn eq(&self, other: &Self) -> bool {
        self.rank(other) == Ordering::Equal
    }
}

impl Eq for Hit {}

impl PartialOrd for Hit {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Hit {
    /// Better ranked is greater, so a max-heap of `Reverse` keeps the
    /// worst on top, where it is evicted.
    fn cmp(&self, other: &Self) -> Ordering {
        other.rank(self)
    }
}

/// The best `limit` hits seen.
#[derive(Debug)]
struct Top {
    limit: usize,
    heap: BinaryHeap<Reverse<Hit>>,
}

impl Top {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            heap: BinaryHeap::with_capacity(limit + 1),
        }
    }

    /// Whether a hit of `bytes` could be kept, before its path is built.
    fn wants(&self, bytes: u64) -> bool {
        self.limit > 0
            && (self.heap.len() < self.limit
                || self
                    .heap
                    .peek()
                    .is_some_and(|Reverse(worst)| bytes >= worst.bytes))
    }

    fn push(&mut self, hit: Hit) {
        self.heap.push(Reverse(hit));
        if self.heap.len() > self.limit {
            self.heap.pop();
        }
    }

    fn into_sorted(self) -> Vec<Hit> {
        let mut hits: Vec<Hit> =
            self.heap.into_iter().map(|Reverse(hit)| hit).collect();
        hits.sort_by(Hit::rank);
        hits
    }
}

/// The one walk over a finished tree: it fills the lists and every tally.
/// Lists honour the query's depth and filters; tallies describe the whole
/// root and honour neither.
#[derive(Debug)]
struct Walk<'a> {
    query: &'a Query,
    names: Exclude,
    cutoff: Option<i64>,
    now: i64,
    total: u64,
    dirs: Top,
    files: Top,
    matched_dirs: u64,
    matched_files: Tally,
    by_category: [Tally; CATEGORIES.len()],
    /// Bytes and files by age, plus one for unknown, last.
    by_age: [Tally; AGES.len() + 1],
    reclaim_by: [Tally; RECLAIMS.len()],
    reclaim_top: Top,
}

impl<'a> Walk<'a> {
    fn new(query: &'a Query, names: Exclude, now: i64, total: u64) -> Self {
        let reclaim_top = if query.sections.reclaimable() {
            query.top
        } else {
            0
        };
        Self {
            query,
            names,
            cutoff: query.filters.older_than.map(|older| older.cutoff(now)),
            now,
            total,
            dirs: Top::new(if query.sections.dirs { query.top } else { 0 }),
            files: Top::new(if query.sections.files { query.top } else { 0 }),
            matched_dirs: 0,
            matched_files: Tally::default(),
            by_category: [Tally::default(); CATEGORIES.len()],
            by_age: [Tally::default(); AGES.len() + 1],
            reclaim_by: [Tally::default(); RECLAIMS.len()],
            reclaim_top: Top::new(reclaim_top),
        }
    }

    /// Visit the children of `node`, which sits at `path` and is of kind
    /// `category`; the children are at `depth` below the root.
    /// `list_dirs` is off beneath a directory already listed as an
    /// outermost match.
    fn visit(
        &mut self,
        node: &Node,
        path: &mut PathBuf,
        depth: usize,
        category: Category,
        list_dirs: bool,
    ) {
        let listing = self.query.depth.is_none_or(|limit| depth <= limit);
        for child in &node.children {
            path.push(&*child.name);
            if child.kind == NodeKind::Directory {
                // The window's rule without its guess for unknown top-level
                // names: a directory is what its own name says, or what
                // holds it. The guess colours `~/Library` by its `Caches`,
                // which is a fine colour and a wrong total.
                let kind = category_of_name(&child.name)
                    .or_else(|| is_git_store(child).then_some(Category::Git))
                    .unwrap_or(category);
                // Reclaim reasons are inherited, so the outermost one is
                // where the parent has none: those never overlap.
                if let Some(reason) = child.reclaim
                    && node.reclaim.is_none()
                {
                    self.reclaim_by[reclaim_index(reason)].add(child.bytes, 1);
                    if self.reclaim_top.wants(child.bytes) {
                        self.reclaim_top
                            .push(Hit::new(child, path, depth, kind));
                    }
                }
                let listed = list_dirs
                    && listing
                    && self.passes(child, path, kind, false);
                if listed {
                    self.matched_dirs += 1;
                    if self.dirs.wants(child.bytes) {
                        self.dirs.push(Hit::new(child, path, depth, kind));
                    }
                }
                let below = list_dirs
                    && !(listed && self.query.filters.outermost_dirs());
                self.visit(child, path, depth + 1, kind, below);
            } else {
                self.tally_leaf(child, category);
                if child.kind == NodeKind::File
                    && listing
                    && self.passes(child, path, category, true)
                {
                    self.matched_files.add(child.bytes, 1);
                    if self.files.wants(child.bytes) {
                        self.files.push(Hit::new(child, path, depth, category));
                    }
                }
            }
            path.pop();
        }
    }

    /// Count a file, link or special file into the breakdowns. Only leaves
    /// hold bytes of their own, so these sum to the root's total.
    fn tally_leaf(&mut self, leaf: &Node, category: Category) {
        let files = u64::from(leaf.kind == NodeKind::File);
        self.by_category[category_index(category)].add(leaf.bytes, files);
        let age =
            age_days(leaf.modified, self.now).map_or(AGES.len(), |days| {
                AGES.iter()
                    .position(|(_, under)| days < *under)
                    .unwrap_or(AGES.len() - 1)
            });
        self.by_age[age].add(leaf.bytes, files);
    }

    fn passes(
        &self,
        node: &Node,
        path: &Path,
        category: Category,
        file: bool,
    ) -> bool {
        let filters = &self.query.filters;
        if filters.min_size.is_some_and(|min| node.bytes < min) {
            return false;
        }
        if let Some(cutoff) = self.cutoff
            && (node.modified == 0 || node.modified >= cutoff)
        {
            return false;
        }
        if file && !filters.extensions.is_empty() {
            let name = node.name.to_ascii_lowercase();
            let matched = filters.extensions.iter().any(|extension| {
                name.len() > extension.len() + 1
                    && name.ends_with(extension.as_str())
                    && name.as_bytes()[name.len() - extension.len() - 1] == b'.'
            });
            if !matched {
                return false;
            }
        }
        if !self.names.is_empty()
            && !self.names.matches(&node.name, || path.to_path_buf())
        {
            return false;
        }
        if filters.category.is_some_and(|wanted| category != wanted) {
            return false;
        }
        match filters.reclaim {
            None => true,
            Some(ReclaimFilter::Any) => node.reclaim.is_some(),
            Some(ReclaimFilter::One(reason)) => node.reclaim == Some(reason),
        }
    }

    fn breakdown_json(&self, tree: &Node, root: &Path, now: i64) -> Value {
        let total = self.total;
        let row = |bytes: u64, files: u64| {
            json!({
                "bytes": bytes,
                "size": human_bytes(bytes),
                "percent": percent(bytes, total),
                "files": files,
            })
        };
        let with = |key: &str, label: &str, mut value: Value| {
            let mut object = Map::new();
            object.insert(key.to_owned(), label.into());
            if let Value::Object(rest) = value.take() {
                object.extend(rest);
            }
            Value::Object(object)
        };

        // By bytes, whatever the tree is ordered by: the window ranks its
        // tree by file count when asked to.
        let mut by_bytes: Vec<&Node> = tree.children.iter().collect();
        by_bytes
            .sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.name.cmp(&b.name)));
        let shown = self.query.top.min(by_bytes.len());
        let (top, rest) = by_bytes.split_at(shown);
        let mut children: Vec<Value> = top
            .iter()
            .map(|child| {
                let kind = if child.is_dir() {
                    category_of_name(&child.name)
                        .or_else(|| {
                            is_git_store(child).then_some(Category::Git)
                        })
                        .unwrap_or(Category::Other)
                } else {
                    Category::Other
                };
                Hit::new(child, &root.join(&*child.name), 1, kind)
                    .json(total, now)
            })
            .collect();
        children.sort_by_key(|entry| Reverse(entry["bytes"].as_u64()));
        let rest_bytes: u64 = rest.iter().map(|child| child.bytes).sum();
        let rest_files: u64 = rest.iter().map(|child| child.files).sum();

        let mut by_category: Vec<(usize, Tally)> =
            self.by_category.iter().copied().enumerate().collect();
        by_category.sort_by_key(|(_, tally)| Reverse(tally.bytes));
        let by_category: Vec<Value> = by_category
            .into_iter()
            .filter(|(_, tally)| tally.bytes > 0 || tally.count > 0)
            .map(|(index, tally)| {
                with(
                    "category",
                    category_slug(CATEGORIES[index]),
                    row(tally.bytes, tally.count),
                )
            })
            .collect();

        let labels = AGES.iter().map(|(label, _)| *label).chain(["unknown"]);
        let by_age: Vec<Value> = labels
            .zip(self.by_age)
            .map(|(label, tally)| {
                with("age", label, row(tally.bytes, tally.count))
            })
            .collect();

        json!({
            "note": "Each list divides total.bytes into parts that do not \
                     overlap; its bytes add up to the total.",
            "children": children,
            "children_rest": {
                "count": rest.len(),
                "bytes": rest_bytes,
                "size": human_bytes(rest_bytes),
                "percent": percent(rest_bytes, total),
                "files": rest_files,
            },
            "by_category": by_category,
            "by_age": by_age,
        })
    }

    fn reclaimable_json(
        &mut self,
        volume: Option<SpaceInfo>,
        now: i64,
    ) -> Value {
        let total = self.total;
        let bytes: u64 = self.reclaim_by.iter().map(|tally| tally.bytes).sum();
        let places: u64 = self.reclaim_by.iter().map(|tally| tally.count).sum();
        let mut by_reason: Vec<(usize, Tally)> = self
            .reclaim_by
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, tally)| tally.count > 0)
            .collect();
        by_reason.sort_by_key(|(_, tally)| Reverse(tally.bytes));
        let by_reason: Vec<Value> = by_reason
            .into_iter()
            .map(|(index, tally)| {
                let reason = RECLAIMS[index];
                json!({
                    "reason": reclaim_slug(reason),
                    "bytes": tally.bytes,
                    "size": human_bytes(tally.bytes),
                    "percent": percent(tally.bytes, total),
                    "places": tally.count,
                    "description": reclaim_description(reason),
                })
            })
            .collect();
        let largest: Vec<Value> =
            std::mem::replace(&mut self.reclaim_top, Top::new(0))
                .into_sorted()
                .iter()
                .map(|hit| hit.json(total, now))
                .collect();
        let available_after = volume.map(|info| {
            let after = info.available.saturating_add(bytes);
            json!({
                "bytes": after,
                "size": human_bytes(after),
                "percent_of_capacity": percent(after, info.total),
            })
        });
        json!({
            "bytes": bytes,
            "size": human_bytes(bytes),
            "percent": percent(bytes, total),
            "places": places,
            "available_after": available_after,
            "note": "Space disktree recognises as regenerable or disposable, \
                     counted once at the outermost folder. A hint to weigh, \
                     not a promise: check before removing anything.",
            "by_reason": by_reason,
            "largest": largest,
        })
    }

    fn matched_json(&self) -> Value {
        let mut matched = Map::new();
        if self.query.sections.dirs {
            matched.insert("directories".into(), self.matched_dirs.into());
        }
        if self.query.sections.files {
            let files = self.matched_files;
            matched.insert("files".into(), files.count.into());
            matched.insert("file_bytes".into(), files.bytes.into());
            matched.insert("file_size".into(), human_bytes(files.bytes).into());
            matched.insert(
                "file_percent".into(),
                percent(files.bytes, self.total).into(),
            );
        }
        Value::Object(matched)
    }
}

fn category_index(category: Category) -> usize {
    CATEGORIES
        .iter()
        .position(|known| *known == category)
        .unwrap_or(CATEGORIES.len() - 1)
}

fn reclaim_index(reclaim: Reclaim) -> usize {
    RECLAIMS
        .iter()
        .position(|known| *known == reclaim)
        .unwrap_or_default()
}

/// A shared scan, as the MCP server caches it.
pub type SharedScan = Arc<Scanned>;

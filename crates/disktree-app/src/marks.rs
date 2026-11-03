//! The set of paths marked for removal.
//!
//! Marks are keyed by absolute path, not by tree position: the tree is
//! re-scanned after a removal, and a mark must survive that (or be reported as
//! gone) rather than silently pointing at a different node.

use std::collections::BTreeSet;
use std::ops::Bound::{Excluded, Unbounded};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use disktree_core::path_style::PathStyle;
use disktree_core::removal::Target;
use disktree_core::tree::{Facts, Metric, Node, Tree};
use rustc_hash::{FxHashMap, FxHashSet};

#[cfg(test)]
pub mod work {
    use std::cell::Cell;

    thread_local! {
        pub static RESOLUTION: Cell<usize> = const { Cell::new(0) };
        pub static FRAMES: Cell<usize> = const { Cell::new(0) };
        pub static RENDERS: Cell<usize> = const { Cell::new(0) };
        pub static PLANS: Cell<usize> = const { Cell::new(0) };
        pub static REFRESHES: Cell<usize> = const { Cell::new(0) };
        pub static TILES: Cell<usize> = const { Cell::new(0) };
        pub static MARKING: Cell<usize> = const { Cell::new(0) };
        pub static COMPACTION: Cell<usize> = const { Cell::new(0) };
        pub static REVIEW: Cell<usize> = const { Cell::new(0) };
        pub static REVIEW_ROWS: Cell<usize> = const { Cell::new(0) };
        pub static COVERED_ROWS: Cell<usize> = const { Cell::new(0) };
        pub static BLOCKED_ROWS: Cell<usize> = const { Cell::new(0) };
        pub static DISCARDED: Cell<usize> = const { Cell::new(0) };
    }

    pub fn step() {
        RESOLUTION.set(RESOLUTION.get() + 1);
    }

    pub fn reset() {
        RESOLUTION.set(0);
        FRAMES.set(0);
        RENDERS.set(0);
        PLANS.set(0);
        REFRESHES.set(0);
        TILES.set(0);
        MARKING.set(0);
        COMPACTION.set(0);
        REVIEW.set(0);
        REVIEW_ROWS.set(0);
        COVERED_ROWS.set(0);
        BLOCKED_ROWS.set(0);
        DISCARDED.set(0);
    }
}

/// Marked paths, in the order they were marked.
#[derive(Debug, Default)]
pub struct Marks {
    items: Vec<Target>,
    // Path's component ordering makes descendants contiguous. The same
    // index answers exact membership and whether a parent has any marks
    // inside, without storing ancestor counts or scanning every mark.
    index: BTreeSet<IndexedPath>,
    resolved: Option<ResolvedMarks>,
}

/// Retain only the allocation identity: a rescan must not keep the old tree
/// alive while a new one is being displayed.
#[derive(Debug)]
struct ResolvedMarks {
    tree: Weak<Tree>,
    root_path: PathBuf,
    metric: Metric,
    style: PathStyle,
    crumbs: Arc<FxHashSet<Vec<usize>>>,
}

// Component ordering keeps descendants contiguous in both namespaces. Keep
// the original spelling as the lookup value, including native non-UTF-8 paths.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PathKey {
    Native(PathBuf),
    Posix(Vec<String>),
}

impl PathKey {
    fn new(path: &Path, style: PathStyle) -> Self {
        match style {
            PathStyle::Native => Self::Native(path.to_path_buf()),
            PathStyle::Posix => Self::Posix(
                path.to_str()
                    .unwrap_or_default()
                    .split('/')
                    .map(str::to_owned)
                    .collect(),
            ),
        }
    }
}

#[derive(Debug)]
struct IndexedPath {
    key: PathKey,
    path: PathBuf,
}

impl std::borrow::Borrow<PathKey> for IndexedPath {
    fn borrow(&self) -> &PathKey {
        &self.key
    }
}

impl PartialEq for IndexedPath {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}
impl Eq for IndexedPath {}
impl PartialOrd for IndexedPath {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for IndexedPath {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key.cmp(&other.key)
    }
}

impl Marks {
    pub fn contains(&self, path: &Path, style: PathStyle) -> bool {
        self.index.contains(&PathKey::new(path, style))
    }

    /// Marked ancestor with the shortest stored spelling, excluding the
    /// path itself: the outermost one for paths built by the scan. Preserve
    /// that choice even for raw queues with redundant path separators.
    pub fn ancestor(&self, path: &Path, style: PathStyle) -> Option<&Path> {
        style
            .ancestors(path)
            .skip(1)
            .filter_map(|ancestor| {
                #[cfg(test)]
                work::MARKING.set(work::MARKING.get() + 1);
                self.index.get(&PathKey::new(ancestor, style))
            })
            .min_by_key(|ancestor| ancestor.path.as_os_str().len())
            .map(|entry| entry.path.as_path())
    }

    /// Drop strict descendants in one stable compaction, retaining the
    /// parent's mark and unrelated marks in their existing order.
    pub fn absorb(&mut self, path: &Path, style: PathStyle) -> usize {
        #[cfg(test)]
        work::MARKING.set(work::MARKING.get() + 1);
        let has_descendant = self
            .index
            .range::<PathKey, _>((
                Excluded(PathKey::new(path, style)),
                Unbounded,
            ))
            .next()
            .is_some_and(|next| style.is_below(&next.path, path));
        // Most insertions are disjoint; proving that from the index keeps
        // queuing M paths from doing M full scans of the ordered item list.
        if !has_descendant {
            return 0;
        }
        self.resolved = None;
        let before = self.items.len();
        let index = &mut self.index;
        self.items.retain(|item| {
            #[cfg(test)]
            work::COMPACTION.set(work::COMPACTION.get() + 1);
            if style.is_below(&item.path, path) {
                #[cfg(test)]
                work::MARKING.set(work::MARKING.get() + 1);
                index.remove(&PathKey::new(&item.path, style));
                false
            } else {
                true
            }
        });
        before - self.items.len()
    }

    pub fn items(&self) -> &[Target] {
        &self.items
    }

    /// Bytes of the items strictly below `path`, as they were recorded.
    pub fn below(&self, path: &Path, style: PathStyle) -> u64 {
        // Descendants sort right after `path`: walk only them.
        let mut below = self
            .index
            .range::<PathKey, _>((
                Excluded(PathKey::new(path, style)),
                Unbounded,
            ))
            .take_while(|next| style.is_below(&next.path, path))
            .map(|next| next.path.as_path())
            .peekable();
        if below.peek().is_none() {
            return 0;
        }
        let below: FxHashSet<&Path> = below.collect();
        self.items
            .iter()
            .filter(|item| below.contains(item.path.as_path()))
            .map(|item| item.bytes)
            .sum()
    }

    /// Take `gone`'s bytes out of every item that holds some of it: a
    /// directory marked after part of it was removed frees only the rest.
    pub fn discount(&mut self, gone: &Self, style: PathStyle) {
        if gone.is_empty() {
            return;
        }
        for item in &mut self.items {
            item.bytes =
                item.bytes.saturating_sub(gone.below(&item.path, style));
        }
    }

    pub const fn len(&self) -> usize {
        self.items.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Mark `target`, or unmark it when it is already marked.
    ///
    /// Returns `true` when the path ended up marked.
    pub fn toggle(&mut self, target: Target, style: PathStyle) -> bool {
        self.resolved = None;
        if self.index.remove(&PathKey::new(&target.path, style)) {
            self.items.retain(|item| {
                PathKey::new(&item.path, style)
                    != PathKey::new(&target.path, style)
            });
            false
        } else {
            self.index.insert(IndexedPath {
                key: PathKey::new(&target.path, style),
                path: target.path.clone(),
            });
            self.items.push(target);
            true
        }
    }

    pub fn remove(&mut self, path: &Path, style: PathStyle) {
        if self.index.remove(&PathKey::new(path, style)) {
            self.resolved = None;
            self.items.retain(|item| {
                #[cfg(test)]
                work::COMPACTION.set(work::COMPACTION.get() + 1);
                PathKey::new(&item.path, style) != PathKey::new(path, style)
            });
        }
    }

    pub fn clear(&mut self) {
        self.resolved = None;
        self.items.clear();
        self.index.clear();
    }

    /// Re-read sizes from a freshly scanned tree. Missing paths stay marked
    /// with zero bytes, so they cannot claim space that is already gone.
    pub fn refresh(
        &mut self,
        root_path: &Path,
        root: Node<'_>,
        _metric: Metric,
        style: PathStyle,
    ) {
        #[cfg(test)]
        work::REFRESHES.set(work::REFRESHES.get() + 1);
        self.resolved = None;
        let mut resolver = Resolver::new(root_path, root, style);
        let mut resolved = Vec::with_capacity(self.items.len());
        for item in &self.items {
            #[cfg(test)]
            work::step();
            match resolver.resolve(&item.path) {
                Some((_, node)) => resolved.push(Target {
                    bytes: node.bytes,
                    is_dir: node.is_dir(),
                    hidden: is_hidden(&item.path, style),
                    path: item.path.clone(),
                }),
                None => resolved.push(Target {
                    bytes: 0,
                    ..item.clone()
                }),
            }
        }
        self.items = resolved;
    }

    /// Crumbs are display state, never removal authorization. Rebuild on every
    /// mark mutation, refresh, tree replacement, root change or metric change.
    pub fn crumbs(
        &mut self,
        root_path: &Path,
        tree: &Arc<Tree>,
        metric: Metric,
        style: PathStyle,
    ) -> Arc<FxHashSet<Vec<usize>>> {
        if let Some(resolved) = &self.resolved
            && resolved.tree.as_ptr() == Arc::as_ptr(tree)
            && style.same(&resolved.root_path, root_path)
            && resolved.metric == metric
            && resolved.style == style
        {
            return Arc::clone(&resolved.crumbs);
        }
        let mut resolver = Resolver::new(root_path, tree.root(), style);
        let mut crumbs = FxHashSet::default();
        for item in &self.items {
            #[cfg(test)]
            work::step();
            if let Some((path, _)) = resolver.resolve(&item.path) {
                crumbs.insert(path);
            }
        }
        let crumbs = Arc::new(crumbs);
        self.resolved = Some(ResolvedMarks {
            tree: Arc::downgrade(tree),
            root_path: root_path.to_path_buf(),
            metric,
            style,
            crumbs: Arc::clone(&crumbs),
        });
        crumbs
    }
}

/// A batch of path lookups indexes each visited directory's children once.
/// Work is O(S + M × D) expected hash operations: S sibling entries across
/// visited directories, M paths and D components per path. Unvisited subtrees
/// cost nothing, and no index is stored on the millions of scan nodes.
#[derive(Debug)]
pub struct Resolver<'a> {
    root_path: &'a Path,
    root: Node<'a>,
    style: PathStyle,
    /// Keyed by where a node's facts live, which is unique per node in
    /// one tree: `Node` is a by-value cursor with no identity of its own.
    indexes: FxHashMap<*const Facts, FxHashMap<&'a str, usize>>,
}

impl<'a> Resolver<'a> {
    pub fn new(root_path: &'a Path, root: Node<'a>, style: PathStyle) -> Self {
        Self {
            root_path,
            root,
            style,
            indexes: FxHashMap::default(),
        }
    }

    /// Compare whole components with the scan's existing case-sensitive,
    /// lossy name semantics. The first equal child wins, as in a linear walk.
    pub fn resolve(&mut self, path: &Path) -> Option<(Vec<usize>, Node<'a>)> {
        let relative = self.style.relative(self.root_path, path)?;
        let mut node = self.root;
        let mut crumbs = Vec::new();
        for name in relative {
            #[cfg(test)]
            work::step();
            let children = self
                .indexes
                .entry(std::ptr::from_ref(node.facts()))
                .or_insert_with(|| {
                    let mut children = FxHashMap::default();
                    for (index, child) in node.children().iter().enumerate() {
                        #[cfg(test)]
                        work::step();
                        children.entry(child.name()).or_insert(index);
                    }
                    children
                });
            let index = *children.get(name.as_ref())?;
            crumbs.push(index);
            node = node.children().at(index);
        }
        Some((crumbs, node))
    }
}

/// A dotfile or dot-directory by name.
pub fn is_hidden(path: &Path, style: PathStyle) -> bool {
    style
        .file_name(path)
        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
}

/// Shorten a path for display: `~` for the home directory, and the path with
/// the home prefix replaced when it is below it. The separator is the scanned
/// machine's, so Windows shows `~\AppData\Local`, not `~/AppData\Local`, and
/// a Unix host shows `~/.cache` on every client.
pub fn display_path(
    path: &Path,
    home: Option<&Path>,
    style: PathStyle,
) -> String {
    let separator = style.separator();
    match home.and_then(|home| style.relative(home, path)) {
        Some(names) if names.is_empty() => "~".to_string(),
        Some(names) => names.iter().fold("~".to_string(), |mut text, name| {
            text.push(separator);
            text.push_str(name);
            text
        }),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disktree_core::tree::{Draft, NodeKind, aggregate};

    fn file(name: &str, bytes: u64) -> Draft {
        Draft::entry(name, NodeKind::File, bytes)
    }

    fn draft() -> Draft {
        let mut root = Draft::directory("home");
        let mut cache = Draft::directory(".cache");
        cache.children.push(file("blob.bin", 900));
        root.children.push(cache);
        root.children.push(file("notes.bin", 100));
        root
    }

    fn tree() -> Tree {
        let mut root = Tree::from(draft());
        aggregate(&mut root, Metric::Bytes);
        root
    }

    #[test]
    fn below_sums_only_descendants_and_discount_leaves_the_rest() {
        let mut gone = Marks::default();
        gone.toggle(target("/home/tobi/a/b", 30), PathStyle::Native);
        gone.toggle(target("/home/tobi/a/c/d", 20), PathStyle::Native);
        gone.toggle(target("/home/tobi/ab", 7), PathStyle::Native);
        let a = Path::new("/home/tobi/a");
        assert_eq!(gone.below(a, PathStyle::Native), 50, "not the sibling ab");
        assert_eq!(
            gone.below(Path::new("/home/tobi/a/b"), PathStyle::Native),
            0
        );
        let mut marks = Marks::default();
        marks.toggle(target("/home/tobi/a", 100), PathStyle::Native);
        marks.discount(&gone, PathStyle::Native);
        assert_eq!(marks.items()[0].bytes, 50);
    }

    fn target(path: &str, bytes: u64) -> Target {
        Target {
            path: PathBuf::from(path),
            bytes,
            is_dir: false,
            hidden: false,
        }
    }

    #[test]
    fn refresh_resolution_budget() {
        let measure = |count| {
            let root_path = std::env::temp_dir().join("marks-budget");
            let mut root = Draft::directory("root");
            let mut marks = Marks::default();
            for index in 0..count {
                let name = format!("folder-{index:05}");
                let mut dir = Draft::directory(name.as_str());
                dir.children.push(file("payload", 1));
                root.children.push(dir);
                marks.toggle(
                    Target {
                        path: root_path.join(name).join("payload"),
                        bytes: 0,
                        is_dir: false,
                        hidden: false,
                    },
                    PathStyle::Native,
                );
            }
            let root = Tree::from(root);
            work::reset();
            let start = std::time::Instant::now();
            for _ in 0..4 {
                marks.refresh(
                    &root_path,
                    root.root(),
                    Metric::Bytes,
                    PathStyle::Native,
                );
            }
            let elapsed = start.elapsed();
            let operations = work::RESOLUTION.get();
            assert!(marks.items().iter().all(|item| item.bytes == 1));
            eprintln!(
                "refresh M={count} N={} R=4 work={operations} wall={elapsed:?}",
                count * 2
            );
            operations
        };
        let small = measure(128);
        let large = measure(1024);
        assert!(large <= small * 16, "8x marks/folders: {small} -> {large}");
        assert!(large <= 1024 * 4 * 16, "absolute work cap: {large}");
    }

    #[test]
    fn cached_crumbs_follow_every_invalidation_boundary() {
        let root_path = Path::new("/home/tobi");
        let mut root = Arc::new(tree());
        let mut marks = Marks::default();
        marks.toggle(target("/home/tobi/.cache", 0), PathStyle::Native);
        let first =
            marks.crumbs(root_path, &root, Metric::Bytes, PathStyle::Native);
        assert!(first.contains(&vec![0]));
        work::reset();
        assert!(Arc::ptr_eq(
            &first,
            &marks.crumbs(root_path, &root, Metric::Bytes, PathStyle::Native)
        ));
        assert_eq!(
            work::RESOLUTION.get(),
            0,
            "unchanged frames need no lookup"
        );
        assert_eq!(
            Arc::strong_count(&root),
            1,
            "the cache must not retain a scan"
        );

        marks.toggle(target("/home/tobi/notes.bin", 0), PathStyle::Native);
        assert_eq!(
            marks
                .crumbs(root_path, &root, Metric::Bytes, PathStyle::Native)
                .len(),
            2
        );
        marks.remove(Path::new("/home/tobi/.cache"), PathStyle::Native);
        assert!(
            marks
                .crumbs(root_path, &root, Metric::Bytes, PathStyle::Native)
                .contains(&vec![1])
        );
        marks.clear();
        assert!(
            marks
                .crumbs(root_path, &root, Metric::Bytes, PathStyle::Native)
                .is_empty()
        );
        marks.toggle(target("/home/tobi/notes.bin", 0), PathStyle::Native);
        marks.toggle(target("/home/tobi/notes.bin", 0), PathStyle::Native);
        assert!(
            marks
                .crumbs(root_path, &root, Metric::Bytes, PathStyle::Native)
                .is_empty()
        );
        marks.toggle(target("/home/tobi/notes.bin", 0), PathStyle::Native);

        let before =
            marks.crumbs(root_path, &root, Metric::Bytes, PathStyle::Native);
        let files =
            marks.crumbs(root_path, &root, Metric::Files, PathStyle::Native);
        assert!(!Arc::ptr_eq(&before, &files), "metric changes invalidate");
        marks.refresh(root_path, root.root(), Metric::Files, PathStyle::Native);
        let refreshed =
            marks.crumbs(root_path, &root, Metric::Files, PathStyle::Native);
        assert!(!Arc::ptr_eq(&files, &refreshed), "refresh invalidates");
        assert!(
            marks
                .crumbs(
                    Path::new("/elsewhere"),
                    &root,
                    Metric::Files,
                    PathStyle::Native
                )
                .is_empty()
        );
        assert!(
            marks
                .crumbs(root_path, &root, Metric::Files, PathStyle::Native)
                .contains(&vec![1])
        );

        let old_tree = Arc::downgrade(&root);
        // A tree's child lists are fixed once built: reorder a copy's draft.
        let mut replacement = root.root().to_draft();
        replacement.children.reverse();
        root = Arc::new(Tree::from(replacement));
        assert!(old_tree.upgrade().is_none(), "old scan is released");
        assert!(
            marks
                .crumbs(root_path, &root, Metric::Files, PathStyle::Native)
                .contains(&vec![0])
        );
        let mut emptied = root.root().to_draft();
        emptied.children.clear();
        root = Arc::new(Tree::from(emptied));
        assert!(
            marks
                .crumbs(root_path, &root, Metric::Files, PathStyle::Native)
                .is_empty()
        );
    }

    #[test]
    fn resolver_preserves_scan_name_semantics() {
        let root_path = Path::new("/home/tobi");
        // A second entry of the same name after aggregation, as a scan
        // that decoded two names to one lossy text would leave it.
        let mut root = {
            let mut tree = Tree::from(draft());
            aggregate(&mut tree, Metric::Bytes);
            tree.root().to_draft()
        };
        root.children.push(file("notes.bin", 200));
        let root = Tree::from(root);
        let mut resolver =
            Resolver::new(root_path, root.root(), PathStyle::Native);
        let (crumbs, node) = resolver
            .resolve(&root_path.join("notes.bin"))
            .expect("file");
        assert_eq!(crumbs, [1]);
        assert_eq!(node.bytes, 100, "first equal scan name wins");
        assert!(resolver.resolve(&root_path.join("NOTES.BIN")).is_none());
        assert!(resolver.resolve(&root_path.join("notes")).is_none());
        assert!(
            resolver
                .resolve(&root_path.join(".cache/missing"))
                .is_none()
        );
        assert!(resolver.resolve(root_path).expect("root").0.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn resolver_preserves_lossy_non_utf8_components() {
        use std::os::unix::ffi::OsStrExt as _;
        let root_path = Path::new("/synthetic");
        let name = std::ffi::OsStr::from_bytes(b"file-\xff");
        let mut root = Draft::directory("root");
        root.children.push(file(&name.to_string_lossy(), 1));
        let root = Tree::from(root);
        let mut resolver =
            Resolver::new(root_path, root.root(), PathStyle::Native);
        assert_eq!(
            resolver
                .resolve(&root_path.join(name))
                .expect("lossy scan name")
                .0,
            [0]
        );
    }

    #[test]
    fn ordered_marks_preserve_components_case_and_outermost_ancestor() {
        let mut marks = Marks::default();
        for path in [
            "/synthetic/a/child",
            "/synthetic/a",
            "/synthetic/a-b",
            "/synthetic/a.b",
            "/synthetic/ab",
            "/synthetic/A",
        ] {
            marks.toggle(target(path, 1), PathStyle::Native);
        }
        assert_eq!(
            marks.ancestor(
                Path::new("/synthetic/a/child/file"),
                PathStyle::Native
            ),
            Some(Path::new("/synthetic/a"))
        );
        assert_eq!(
            marks.ancestor(Path::new("/synthetic/a"), PathStyle::Native),
            None
        );
        assert_eq!(
            marks.ancestor(Path::new("/synthetic/a-b/file"), PathStyle::Native),
            Some(Path::new("/synthetic/a-b"))
        );
        assert_eq!(
            marks.ancestor(Path::new("/synthetic/A/file"), PathStyle::Native),
            Some(Path::new("/synthetic/A"))
        );
        assert_eq!(
            marks.absorb(Path::new("/synthetic/a"), PathStyle::Native),
            1
        );
        let expected: Vec<_> = [
            "/synthetic/a",
            "/synthetic/a-b",
            "/synthetic/a.b",
            "/synthetic/ab",
            "/synthetic/A",
        ]
        .map(PathBuf::from)
        .into();
        assert_eq!(
            marks
                .items()
                .iter()
                .map(|item| item.path.clone())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(
            marks
                .items()
                .iter()
                .all(|item| marks.contains(&item.path, PathStyle::Native))
        );
        assert!(
            !marks.contains(Path::new("/synthetic/a/child"), PathStyle::Native)
        );
        marks.remove(Path::new("/synthetic/a"), PathStyle::Native);
        assert_eq!(
            marks.ancestor(Path::new("/synthetic/a/file"), PathStyle::Native),
            None
        );
        assert_eq!(
            marks.absorb(Path::new("/synthetic/a"), PathStyle::Native),
            0,
            "prefix lookalikes stay marked"
        );
        marks.clear();
        assert_eq!(marks.absorb(Path::new("/synthetic"), PathStyle::Native), 0);

        // Raw control clients can supply redundant separators. Retain the
        // original shortest-spelling rule rather than changing notices.
        marks.toggle(
            target("/synthetic/a////////////////////", 1),
            PathStyle::Native,
        );
        marks.toggle(target("/synthetic/a/child", 1), PathStyle::Native);
        assert_eq!(
            marks.ancestor(
                Path::new("/synthetic/a/child/file"),
                PathStyle::Native
            ),
            Some(Path::new("/synthetic/a/child"))
        );
    }

    #[test]
    fn absorption_invalidates_cached_crumbs_and_keeps_missing_paths_indexed() {
        let root_path = Path::new("/home/tobi");
        let root = Arc::new(tree());
        let mut marks = Marks::default();
        marks
            .toggle(target("/home/tobi/.cache/blob.bin", 0), PathStyle::Native);
        marks.toggle(target("/home/tobi/.cache", 0), PathStyle::Native);
        marks.toggle(target("/home/tobi/gone/nested", 100), PathStyle::Native);
        let before =
            marks.crumbs(root_path, &root, Metric::Bytes, PathStyle::Native);
        assert_eq!(before.len(), 2);
        assert_eq!(
            marks.absorb(Path::new("/home/tobi/.cache"), PathStyle::Native),
            1
        );
        let after =
            marks.crumbs(root_path, &root, Metric::Bytes, PathStyle::Native);
        assert!(!Arc::ptr_eq(&before, &after));
        assert_eq!(after.len(), 1);
        assert!(after.contains(&vec![0]));
        marks.refresh(root_path, root.root(), Metric::Files, PathStyle::Native);
        assert!(
            marks.contains(
                Path::new("/home/tobi/gone/nested"),
                PathStyle::Native
            )
        );
        assert_eq!(
            marks.absorb(Path::new("/home/tobi/gone"), PathStyle::Native),
            1
        );
        assert!(
            !marks.contains(
                Path::new("/home/tobi/gone/nested"),
                PathStyle::Native
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn ordered_marks_keep_exact_non_utf8_paths() {
        use std::os::unix::ffi::OsStrExt as _;
        let root = Path::new("/synthetic");
        let parent = root.join(std::ffi::OsStr::from_bytes(b"not-\xff"));
        let child = parent.join("file");
        let mut marks = Marks::default();
        marks.toggle(
            Target {
                path: child.clone(),
                bytes: 1,
                is_dir: false,
                hidden: false,
            },
            PathStyle::Native,
        );
        marks.toggle(
            Target {
                path: parent.clone(),
                bytes: 1,
                is_dir: true,
                hidden: false,
            },
            PathStyle::Native,
        );
        assert_eq!(
            marks.ancestor(&child, PathStyle::Native),
            Some(parent.as_path())
        );
        assert_eq!(marks.absorb(&parent, PathStyle::Native), 1);
        assert!(marks.contains(&parent, PathStyle::Native));
        assert!(!marks.contains(&child, PathStyle::Native));
    }

    #[test]
    fn toggling_marks_and_unmarks() {
        let mut marks = Marks::default();
        assert!(marks.toggle(target("/home/tobi/a", 1), PathStyle::Native));
        assert!(marks.contains(Path::new("/home/tobi/a"), PathStyle::Native));
        assert_eq!(marks.len(), 1);
        assert!(!marks.toggle(target("/home/tobi/a", 1), PathStyle::Native));
        assert!(marks.is_empty());
    }

    #[test]
    fn marking_the_same_path_twice_keeps_one_entry() {
        let mut marks = Marks::default();
        marks.toggle(target("/home/tobi/a", 1), PathStyle::Native);
        marks.toggle(target("/home/tobi/a", 1), PathStyle::Native);
        marks.toggle(target("/home/tobi/a", 1), PathStyle::Native);
        assert_eq!(marks.len(), 1);
    }

    #[test]
    fn refresh_re_reads_sizes_from_a_new_tree() {
        let root_path = Path::new("/home/tobi");
        let root = tree();
        let mut marks = Marks::default();
        marks.toggle(target("/home/tobi/.cache", 0), PathStyle::Native);
        marks.toggle(target("/home/tobi/gone", 500), PathStyle::Native);

        // Removal bytes stay measured bytes even when the treemap shows files.
        for metric in [Metric::Bytes, Metric::Files] {
            marks.refresh(root_path, root.root(), metric, PathStyle::Native);
            assert_eq!(marks.items()[0].bytes, 900);
            assert!(marks.items()[0].is_dir);
            assert!(marks.items()[0].hidden, "the .cache mark is hidden");
            assert_eq!(
                marks.items()[1].bytes,
                0,
                "a path that no longer exists"
            );
        }
    }

    #[test]
    fn find_matches_whole_components_only() {
        let root = tree();
        let mut resolver = Resolver::new(
            Path::new("/home/tobi"),
            root.root(),
            PathStyle::Native,
        );
        assert!(resolver.resolve(Path::new("/home/tobi/.cache")).is_some());
        assert!(
            resolver
                .resolve(Path::new("/home/tobi/.cache/blob.bin"))
                .is_some()
        );
        assert!(resolver.resolve(Path::new("/home/tobi/cache")).is_none());
        assert!(
            Resolver::new(
                Path::new("/elsewhere"),
                root.root(),
                PathStyle::Native
            )
            .resolve(Path::new("/home/tobi/notes.bin"))
            .is_none()
        );
    }

    #[test]
    fn display_path_shortens_the_home_prefix() {
        let home = Path::new("/home/tobi");
        let separator = std::path::MAIN_SEPARATOR;
        assert_eq!(
            display_path(
                &home.join(".cache").join("npm"),
                Some(home),
                PathStyle::Native
            ),
            format!("~{separator}.cache{separator}npm")
        );
        assert_eq!(display_path(home, Some(home), PathStyle::Native), "~");
        assert_eq!(
            display_path(Path::new("/var/log"), Some(home), PathStyle::Native),
            "/var/log"
        );
        assert_eq!(
            display_path(Path::new("/var/log"), None, PathStyle::Native),
            "/var/log"
        );
    }
    /// Vary each input separately: files, marks, path depth and refreshes
    /// must not be conflated with growing siblings and marks together.
    #[test]
    fn refresh_dimension_budget() {
        let measure = |folders: usize,
                       marked: usize,
                       depth: usize,
                       repeats: usize| {
            let root_path = std::env::temp_dir().join("marks-dimensions");
            let mut root = Draft::directory("root");
            let mut marks = Marks::default();
            for index in 0..folders {
                let name = format!("folder-{index:05}");
                let mut child = file("payload", 1);
                let mut relative = PathBuf::from(&name);
                for _ in 1..depth {
                    let mut parent = Draft::directory("nested");
                    parent.children.push(child);
                    child = parent;
                    relative.push("nested");
                }
                let mut folder = Draft::directory(name);
                folder.children.push(child);
                root.children.push(folder);
                relative.push("payload");
                if index >= folders - marked {
                    marks.toggle(
                        Target {
                            path: root_path.join(relative),
                            bytes: 0,
                            is_dir: false,
                            hidden: false,
                        },
                        PathStyle::Native,
                    );
                }
            }
            let root = Tree::from(root);
            work::reset();
            let start = std::time::Instant::now();
            for _ in 0..repeats {
                marks.refresh(
                    &root_path,
                    root.root(),
                    Metric::Bytes,
                    PathStyle::Native,
                );
            }
            let operations = work::RESOLUTION.get();
            assert!(marks.items().iter().all(|item| item.bytes == 1));
            eprintln!(
                "dimensions folders={folders} M={marked} D={} R={repeats} work={operations} wall={:?}",
                depth + 1,
                start.elapsed()
            );
            operations
        };
        let base = measure(128, 8, 1, 1);
        let more_files = measure(1024, 8, 1, 1);
        assert!(more_files <= base * 16);
        let more_marks = measure(1024, 64, 1, 1);
        assert!(more_marks <= more_files * 16);
        let more_depth = measure(128, 8, 15, 1);
        assert!(more_depth <= base * 16);
        let more_refreshes = measure(128, 8, 1, 8);
        assert_eq!(more_refreshes, base * 8);
        assert!(more_files <= 1024 * 8 * 4);
    }
    /// A Unix host's paths read the same on every client: `/` after `~`, and
    /// a name holding `\` is one name, not two.
    #[test]
    fn remote_paths_display_and_resolve_by_posix_rules() {
        let posix = PathStyle::Posix;
        let home = Path::new("/home/tobi");
        assert_eq!(
            display_path(Path::new("/home/tobi/.cache/npm"), Some(home), posix),
            "~/.cache/npm"
        );
        assert_eq!(
            display_path(Path::new("/home/tobix"), Some(home), posix),
            "/home/tobix"
        );
        let mut root = Draft::directory("home");
        root.children.push(file(r"a\b", 7));
        let root = Tree::from(root);
        assert!(
            Resolver::new(home, root.root(), posix)
                .resolve(Path::new(r"/home/tobi/a\b"))
                .is_some()
        );
        assert!(
            Resolver::new(home, root.root(), posix)
                .resolve(Path::new("/home/tobi/a/b"))
                .is_none()
        );
        assert!(!is_hidden(Path::new(r"/srv/x\.hidden"), posix));
        assert!(is_hidden(Path::new("/srv/.cache"), posix));
    }

    /// Marks are keyed by the exact path text, so `/a/b\c` and `/a/b/c`
    /// stay two marks even where `PathBuf` would call them equal.
    #[test]
    fn marks_compare_the_exact_path() {
        let mut marks = Marks::default();
        marks.toggle(target(r"/srv/b\c", 1), PathStyle::Posix);
        assert!(!marks.contains(Path::new("/srv/b/c"), PathStyle::Posix));
        assert!(marks.toggle(target("/srv/b/c", 1), PathStyle::Posix));
        assert_eq!(marks.len(), 2);
        marks.remove(Path::new("/srv/b/c"), PathStyle::Posix);
        assert!(marks.contains(Path::new(r"/srv/b\c"), PathStyle::Posix));
    }

    #[test]
    fn posix_index_absorbs_only_whole_host_components() {
        let style = PathStyle::Posix;
        let mut marks = Marks::default();
        for path in [
            "/srv/a-b/keep",
            "/srv/a/child",
            r"/srv/a\b/keep",
            "/srv/a:b/keep",
            "/srv/a",
        ] {
            marks.toggle(target(path, 1), style);
        }
        assert_eq!(marks.absorb(Path::new("/srv/a"), style), 1);
        let paths: Vec<_> = marks
            .items()
            .iter()
            .map(|item| item.path.to_str().unwrap())
            .collect();
        assert_eq!(
            paths,
            ["/srv/a-b/keep", r"/srv/a\b/keep", "/srv/a:b/keep", "/srv/a"]
        );
        assert_eq!(
            marks
                .ancestor(Path::new("/srv/a/next"), style)
                .unwrap()
                .to_str(),
            Some("/srv/a")
        );
        assert!(marks.ancestor(Path::new(r"/srv/a\b/next"), style).is_none());
        assert_eq!(marks.absorb(Path::new(r"/srv/a\b"), style), 1);
        assert!(marks.contains(Path::new("/srv/a:b/keep"), style));
        assert!(marks.contains(Path::new("/srv/a-b/keep"), style));
    }

    #[test]
    fn posix_refresh_and_crumb_cache_keep_ambiguous_windows_names_distinct() {
        let style = PathStyle::Posix;
        let root_path = Path::new("/srv");
        let mut root = Draft::directory("srv");
        let mut literal = Draft::directory(r"logs\old");
        literal.children.push(file("a:b", 7));
        let mut nested = Draft::directory("logs");
        let mut old = Draft::directory("old");
        old.children.push(file("a:b", 9));
        nested.children.push(old);
        root.children.extend([literal, nested]);
        let mut root = Tree::from(root);
        aggregate(&mut root, Metric::Bytes);
        let root = Arc::new(root);
        let mut marks = Marks::default();
        for path in [r"/srv/logs\old/a:b", "/srv/logs/old/a:b"] {
            marks.toggle(target(path, 0), style);
        }
        marks.refresh(root_path, root.root(), Metric::Bytes, style);
        assert_eq!(
            marks
                .items()
                .iter()
                .map(|item| item.bytes)
                .collect::<Vec<_>>(),
            [7, 9]
        );
        let crumbs = marks.crumbs(root_path, &root, Metric::Bytes, style);
        assert_eq!(crumbs.len(), 2);
        for item in marks.items() {
            let (path, node) = Resolver::new(root_path, root.root(), style)
                .resolve(&item.path)
                .unwrap();
            assert!(crumbs.contains(&path));
            assert_eq!(node.bytes, item.bytes);
        }
        let native =
            marks.crumbs(root_path, &root, Metric::Bytes, PathStyle::Native);
        assert!(
            !Arc::ptr_eq(&crumbs, &native),
            "path rules invalidate the cache"
        );
        assert_eq!(
            marks.crumbs(root_path, &root, Metric::Bytes, style).len(),
            2
        );
    }

    #[test]
    fn posix_crumb_cache_compares_root_text_in_the_host_namespace() {
        let style = PathStyle::Posix;
        let root_path = Path::new(r"/srv/a\b");
        let mut root = Draft::directory("root");
        root.children.push(file("payload", 7));
        let root = Arc::new(Tree::from(root));
        let mut marks = Marks::default();
        marks.toggle(target(r"/srv/a\b/payload", 7), style);
        assert_eq!(
            marks.crumbs(root_path, &root, Metric::Bytes, style).len(),
            1
        );
        assert!(
            marks
                .crumbs(Path::new("/srv/a/b"), &root, Metric::Bytes, style)
                .is_empty()
        );
    }
}

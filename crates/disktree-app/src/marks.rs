//! The set of paths marked for removal.
//!
//! Marks are keyed by absolute path, not by tree position: the tree is
//! re-scanned after a removal, and a mark must survive that (or be reported as
//! gone) rather than silently pointing at a different node.

use std::collections::BTreeSet;
use std::ops::Bound::{Excluded, Unbounded};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use disktree_core::removal::Target;
use disktree_core::tree::{Metric, Node};
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
    }
}

/// Marked paths, in the order they were marked.
#[derive(Debug, Default)]
pub struct Marks {
    items: Vec<Target>,
    // Path's component ordering makes descendants contiguous. The same
    // index answers exact membership and whether a parent has any marks
    // inside, without storing ancestor counts or scanning every mark.
    index: BTreeSet<PathBuf>,
    resolved: Option<ResolvedMarks>,
}

/// Retain only the allocation identity: a rescan must not keep the old tree
/// alive while a new one is being displayed.
#[derive(Debug)]
struct ResolvedMarks {
    tree: Weak<Node>,
    root_path: PathBuf,
    metric: Metric,
    crumbs: Arc<FxHashSet<Vec<usize>>>,
}

impl Marks {
    pub fn contains(&self, path: &Path) -> bool {
        self.index.contains(path)
    }

    /// Marked ancestor with the shortest stored spelling, excluding the
    /// path itself: the outermost one for paths built by the scan. Preserve
    /// that choice even for raw queues with redundant path separators.
    pub fn ancestor(&self, path: &Path) -> Option<&Path> {
        path.ancestors()
            .skip(1)
            .filter_map(|ancestor| {
                #[cfg(test)]
                work::MARKING.set(work::MARKING.get() + 1);
                self.index.get(ancestor)
            })
            .min_by_key(|ancestor| ancestor.as_os_str().len())
            .map(PathBuf::as_path)
    }

    /// Drop strict descendants in one stable compaction, retaining the
    /// parent's mark and unrelated marks in their existing order.
    pub fn absorb(&mut self, path: &Path) -> usize {
        #[cfg(test)]
        work::MARKING.set(work::MARKING.get() + 1);
        let has_descendant = self
            .index
            .range::<Path, _>((Excluded(path), Unbounded))
            .next()
            .is_some_and(|next| next.starts_with(path));
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
            if item.path != path && item.path.starts_with(path) {
                #[cfg(test)]
                work::MARKING.set(work::MARKING.get() + 1);
                index.remove(&item.path);
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

    pub const fn len(&self) -> usize {
        self.items.len()
    }

    pub const fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Mark `target`, or unmark it when it is already marked.
    ///
    /// Returns `true` when the path ended up marked.
    pub fn toggle(&mut self, target: Target) -> bool {
        self.resolved = None;
        if self.index.remove(&target.path) {
            self.items.retain(|item| item.path != target.path);
            false
        } else {
            self.index.insert(target.path.clone());
            self.items.push(target);
            true
        }
    }

    pub fn remove(&mut self, path: &Path) {
        if self.index.remove(path) {
            self.resolved = None;
            self.items.retain(|item| {
                #[cfg(test)]
                work::COMPACTION.set(work::COMPACTION.get() + 1);
                item.path != path
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
    pub fn refresh(&mut self, root_path: &Path, root: &Node, _metric: Metric) {
        #[cfg(test)]
        work::REFRESHES.set(work::REFRESHES.get() + 1);
        self.resolved = None;
        let mut resolver = Resolver::new(root_path, root);
        let mut resolved = Vec::with_capacity(self.items.len());
        for item in &self.items {
            #[cfg(test)]
            work::step();
            match resolver.resolve(&item.path) {
                Some((_, node)) => resolved.push(Target {
                    bytes: node.bytes,
                    is_dir: node.is_dir(),
                    hidden: is_hidden(&item.path),
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
        tree: &Arc<Node>,
        metric: Metric,
    ) -> Arc<FxHashSet<Vec<usize>>> {
        if let Some(resolved) = &self.resolved
            && resolved.tree.as_ptr() == Arc::as_ptr(tree)
            && resolved.root_path == root_path
            && resolved.metric == metric
        {
            return Arc::clone(&resolved.crumbs);
        }
        let mut resolver = Resolver::new(root_path, tree);
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
    root: &'a Node,
    indexes: FxHashMap<*const Node, FxHashMap<&'a str, usize>>,
}

impl<'a> Resolver<'a> {
    pub fn new(root_path: &'a Path, root: &'a Node) -> Self {
        Self {
            root_path,
            root,
            indexes: FxHashMap::default(),
        }
    }

    /// Compare whole components with the scan's existing case-sensitive,
    /// lossy name semantics. The first equal child wins, as in a linear walk.
    pub fn resolve(&mut self, path: &Path) -> Option<(Vec<usize>, &'a Node)> {
        let relative = path.strip_prefix(self.root_path).ok()?;
        let mut node = self.root;
        let mut crumbs = Vec::new();
        for component in relative.components() {
            #[cfg(test)]
            work::step();
            let name = component.as_os_str().to_string_lossy();
            let children = self
                .indexes
                .entry(std::ptr::from_ref(node))
                .or_insert_with(|| {
                    let mut children = FxHashMap::default();
                    for (index, child) in node.children.iter().enumerate() {
                        #[cfg(test)]
                        work::step();
                        children.entry(child.name.as_ref()).or_insert(index);
                    }
                    children
                });
            let index = *children.get(name.as_ref())?;
            crumbs.push(index);
            node = &node.children[index];
        }
        Some((crumbs, node))
    }
}

/// A dotfile or dot-directory by name.
pub fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
}

/// Shorten a path for display: `~` for the home directory, and the path with
/// the home prefix replaced when it is below it. The separator after `~` is
/// the platform's, so Windows shows `~\AppData\Local`, not `~/AppData\Local`.
pub fn display_path(path: &Path, home: Option<&Path>) -> String {
    match home
        .and_then(|home| path.strip_prefix(home).ok().map(|rest| (home, rest)))
    {
        Some((_, rest)) if rest.as_os_str().is_empty() => "~".to_string(),
        Some((_, rest)) => {
            format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display())
        }
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use disktree_core::tree::{NodeKind, aggregate};

    fn file(name: &str, bytes: u64) -> Node {
        Node::entry(name, NodeKind::File, bytes)
    }

    fn tree() -> Node {
        let mut root = Node::directory("home");
        let mut cache = Node::directory(".cache");
        cache.children.push(file("blob.bin", 900));
        root.children.push(cache);
        root.children.push(file("notes.bin", 100));
        aggregate(&mut root, Metric::Bytes);
        root
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
            let mut root = Node::directory("root");
            let mut marks = Marks::default();
            for index in 0..count {
                let name = format!("folder-{index:05}");
                let mut dir = Node::directory(name.as_str());
                dir.children.push(file("payload", 1));
                root.children.push(dir);
                marks.toggle(Target {
                    path: root_path.join(name).join("payload"),
                    bytes: 0,
                    is_dir: false,
                    hidden: false,
                });
            }
            work::reset();
            let start = std::time::Instant::now();
            for _ in 0..4 {
                marks.refresh(&root_path, &root, Metric::Bytes);
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
        marks.toggle(target("/home/tobi/.cache", 0));
        let first = marks.crumbs(root_path, &root, Metric::Bytes);
        assert!(first.contains(&vec![0]));
        work::reset();
        assert!(Arc::ptr_eq(
            &first,
            &marks.crumbs(root_path, &root, Metric::Bytes)
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

        marks.toggle(target("/home/tobi/notes.bin", 0));
        assert_eq!(marks.crumbs(root_path, &root, Metric::Bytes).len(), 2);
        marks.remove(Path::new("/home/tobi/.cache"));
        assert!(
            marks
                .crumbs(root_path, &root, Metric::Bytes)
                .contains(&vec![1])
        );
        marks.clear();
        assert!(marks.crumbs(root_path, &root, Metric::Bytes).is_empty());
        marks.toggle(target("/home/tobi/notes.bin", 0));
        marks.toggle(target("/home/tobi/notes.bin", 0));
        assert!(marks.crumbs(root_path, &root, Metric::Bytes).is_empty());
        marks.toggle(target("/home/tobi/notes.bin", 0));

        let before = marks.crumbs(root_path, &root, Metric::Bytes);
        let files = marks.crumbs(root_path, &root, Metric::Files);
        assert!(!Arc::ptr_eq(&before, &files), "metric changes invalidate");
        marks.refresh(root_path, &root, Metric::Files);
        let refreshed = marks.crumbs(root_path, &root, Metric::Files);
        assert!(!Arc::ptr_eq(&files, &refreshed), "refresh invalidates");
        assert!(
            marks
                .crumbs(Path::new("/elsewhere"), &root, Metric::Files)
                .is_empty()
        );
        assert!(
            marks
                .crumbs(root_path, &root, Metric::Files)
                .contains(&vec![1])
        );

        let old_tree = Arc::downgrade(&root);
        let mut replacement = (*root).clone();
        replacement.children.reverse();
        root = Arc::new(replacement);
        assert!(old_tree.upgrade().is_none(), "old scan is released");
        assert!(
            marks
                .crumbs(root_path, &root, Metric::Files)
                .contains(&vec![0])
        );
        Arc::make_mut(&mut root).children.clear();
        assert!(marks.crumbs(root_path, &root, Metric::Files).is_empty());
    }

    #[test]
    fn resolver_preserves_scan_name_semantics() {
        let root_path = Path::new("/home/tobi");
        let mut root = tree();
        root.children.push(file("notes.bin", 200));
        let mut resolver = Resolver::new(root_path, &root);
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
        let mut root = Node::directory("root");
        root.children.push(file(&name.to_string_lossy(), 1));
        let mut resolver = Resolver::new(root_path, &root);
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
            marks.toggle(target(path, 1));
        }
        assert_eq!(
            marks.ancestor(Path::new("/synthetic/a/child/file")),
            Some(Path::new("/synthetic/a"))
        );
        assert_eq!(marks.ancestor(Path::new("/synthetic/a")), None);
        assert_eq!(
            marks.ancestor(Path::new("/synthetic/a-b/file")),
            Some(Path::new("/synthetic/a-b"))
        );
        assert_eq!(
            marks.ancestor(Path::new("/synthetic/A/file")),
            Some(Path::new("/synthetic/A"))
        );
        assert_eq!(marks.absorb(Path::new("/synthetic/a")), 1);
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
        assert!(marks.items().iter().all(|item| marks.contains(&item.path)));
        assert!(!marks.contains(Path::new("/synthetic/a/child")));
        marks.remove(Path::new("/synthetic/a"));
        assert_eq!(marks.ancestor(Path::new("/synthetic/a/file")), None);
        assert_eq!(
            marks.absorb(Path::new("/synthetic/a")),
            0,
            "prefix lookalikes stay marked"
        );
        marks.clear();
        assert_eq!(marks.absorb(Path::new("/synthetic")), 0);

        // Raw control clients can supply redundant separators. Retain the
        // original shortest-spelling rule rather than changing notices.
        marks.toggle(target("/synthetic/a////////////////////", 1));
        marks.toggle(target("/synthetic/a/child", 1));
        assert_eq!(
            marks.ancestor(Path::new("/synthetic/a/child/file")),
            Some(Path::new("/synthetic/a/child"))
        );
    }

    #[test]
    fn absorption_invalidates_cached_crumbs_and_keeps_missing_paths_indexed() {
        let root_path = Path::new("/home/tobi");
        let root = Arc::new(tree());
        let mut marks = Marks::default();
        marks.toggle(target("/home/tobi/.cache/blob.bin", 0));
        marks.toggle(target("/home/tobi/.cache", 0));
        marks.toggle(target("/home/tobi/gone/nested", 100));
        let before = marks.crumbs(root_path, &root, Metric::Bytes);
        assert_eq!(before.len(), 2);
        assert_eq!(marks.absorb(Path::new("/home/tobi/.cache")), 1);
        let after = marks.crumbs(root_path, &root, Metric::Bytes);
        assert!(!Arc::ptr_eq(&before, &after));
        assert_eq!(after.len(), 1);
        assert!(after.contains(&vec![0]));
        marks.refresh(root_path, &root, Metric::Files);
        assert!(marks.contains(Path::new("/home/tobi/gone/nested")));
        assert_eq!(marks.absorb(Path::new("/home/tobi/gone")), 1);
        assert!(!marks.contains(Path::new("/home/tobi/gone/nested")));
    }

    #[cfg(unix)]
    #[test]
    fn ordered_marks_keep_exact_non_utf8_paths() {
        use std::os::unix::ffi::OsStrExt as _;
        let root = Path::new("/synthetic");
        let parent = root.join(std::ffi::OsStr::from_bytes(b"not-\xff"));
        let child = parent.join("file");
        let mut marks = Marks::default();
        marks.toggle(Target {
            path: child.clone(),
            bytes: 1,
            is_dir: false,
            hidden: false,
        });
        marks.toggle(Target {
            path: parent.clone(),
            bytes: 1,
            is_dir: true,
            hidden: false,
        });
        assert_eq!(marks.ancestor(&child), Some(parent.as_path()));
        assert_eq!(marks.absorb(&parent), 1);
        assert!(marks.contains(&parent));
        assert!(!marks.contains(&child));
    }

    #[test]
    fn toggling_marks_and_unmarks() {
        let mut marks = Marks::default();
        assert!(marks.toggle(target("/home/tobi/a", 1)));
        assert!(marks.contains(Path::new("/home/tobi/a")));
        assert_eq!(marks.len(), 1);
        assert!(!marks.toggle(target("/home/tobi/a", 1)));
        assert!(marks.is_empty());
    }

    #[test]
    fn marking_the_same_path_twice_keeps_one_entry() {
        let mut marks = Marks::default();
        marks.toggle(target("/home/tobi/a", 1));
        marks.toggle(target("/home/tobi/a", 1));
        marks.toggle(target("/home/tobi/a", 1));
        assert_eq!(marks.len(), 1);
    }

    #[test]
    fn refresh_re_reads_sizes_from_a_new_tree() {
        let root_path = Path::new("/home/tobi");
        let root = tree();
        let mut marks = Marks::default();
        marks.toggle(target("/home/tobi/.cache", 0));
        marks.toggle(target("/home/tobi/gone", 500));

        // Removal bytes stay measured bytes even when the treemap shows files.
        for metric in [Metric::Bytes, Metric::Files] {
            marks.refresh(root_path, &root, metric);
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
        let mut resolver = Resolver::new(Path::new("/home/tobi"), &root);
        assert!(resolver.resolve(Path::new("/home/tobi/.cache")).is_some());
        assert!(
            resolver
                .resolve(Path::new("/home/tobi/.cache/blob.bin"))
                .is_some()
        );
        assert!(resolver.resolve(Path::new("/home/tobi/cache")).is_none());
        assert!(
            Resolver::new(Path::new("/elsewhere"), &root)
                .resolve(Path::new("/home/tobi/notes.bin"))
                .is_none()
        );
    }

    #[test]
    fn display_path_shortens_the_home_prefix() {
        let home = Path::new("/home/tobi");
        let separator = std::path::MAIN_SEPARATOR;
        assert_eq!(
            display_path(&home.join(".cache").join("npm"), Some(home)),
            format!("~{separator}.cache{separator}npm")
        );
        assert_eq!(display_path(home, Some(home)), "~");
        assert_eq!(display_path(Path::new("/var/log"), Some(home)), "/var/log");
        assert_eq!(display_path(Path::new("/var/log"), None), "/var/log");
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
            let mut root = Node::directory("root");
            let mut marks = Marks::default();
            for index in 0..folders {
                let name = format!("folder-{index:05}");
                let mut child = file("payload", 1);
                let mut relative = PathBuf::from(&name);
                for _ in 1..depth {
                    let mut parent = Node::directory("nested");
                    parent.children.push(child);
                    child = parent;
                    relative.push("nested");
                }
                let mut folder = Node::directory(name);
                folder.children.push(child);
                root.children.push(folder);
                relative.push("payload");
                if index >= folders - marked {
                    marks.toggle(Target {
                        path: root_path.join(relative),
                        bytes: 0,
                        is_dir: false,
                        hidden: false,
                    });
                }
            }
            work::reset();
            let start = std::time::Instant::now();
            for _ in 0..repeats {
                marks.refresh(&root_path, &root, Metric::Bytes);
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
}

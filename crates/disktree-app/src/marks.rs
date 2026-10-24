//! The set of paths marked for removal.
//!
//! Marks are keyed by absolute path, not by tree position: the tree is
//! re-scanned after a removal, and a mark must survive that (or be reported as
//! gone) rather than silently pointing at a different node.

use std::ffi::OsString;
use std::path::Path;

use disktree_core::path_style::PathStyle;
use disktree_core::removal::Target;
use disktree_core::tree::{Metric, Node};
use rustc_hash::FxHashSet;

/// Marked paths, in the order they were marked.
///
/// The index holds the exact text of each path. `PathBuf` equality on
/// Windows compares components and treats `\` as a separator, so a Unix
/// host's `/a/b\c` would equal `/a/b/c` there.
#[derive(Debug, Default)]
pub struct Marks {
    items: Vec<Target>,
    index: FxHashSet<OsString>,
}

impl Marks {
    pub fn contains(&self, path: &Path) -> bool {
        self.index.contains(path.as_os_str())
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
        if self.index.remove(target.path.as_os_str()) {
            self.items.retain(|item| {
                item.path.as_os_str() != target.path.as_os_str()
            });
            false
        } else {
            self.index.insert(target.path.clone().into_os_string());
            self.items.push(target);
            true
        }
    }

    pub fn remove(&mut self, path: &Path) {
        if self.index.remove(path.as_os_str()) {
            self.items
                .retain(|item| item.path.as_os_str() != path.as_os_str());
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.index.clear();
    }

    /// Re-read sizes from a freshly scanned tree and drop marks whose path no
    /// longer exists, so the tally never claims space that is already gone.
    pub fn refresh(
        &mut self,
        root_path: &Path,
        root: &Node,
        _metric: Metric,
        style: PathStyle,
    ) {
        let mut resolved = Vec::with_capacity(self.items.len());
        for item in &self.items {
            match find(root_path, root, &item.path, style) {
                Some(node) => resolved.push(Target {
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
}

/// Walk the tree to the node at an absolute path. Names are compared one at a
/// time, split by the scanned machine's rules, so a name containing another
/// platform's separator cannot confuse it.
pub fn find<'a>(
    root_path: &Path,
    root: &'a Node,
    path: &Path,
    style: PathStyle,
) -> Option<&'a Node> {
    let mut node = root;
    for name in style.relative(root_path, path)? {
        node = node
            .children
            .iter()
            .find(|child| child.name.as_ref() == name)?;
    }
    Some(node)
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
    use disktree_core::tree::{NodeKind, aggregate};
    use std::path::PathBuf;

    const NATIVE: PathStyle = PathStyle::Native;

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

        marks.refresh(root_path, &root, Metric::Bytes, NATIVE);
        assert_eq!(marks.items()[0].bytes, 900);
        assert!(marks.items()[0].is_dir);
        assert!(marks.items()[0].hidden, "the .cache mark is hidden");
        assert_eq!(marks.items()[1].bytes, 0, "a path that no longer exists");
    }

    #[test]
    fn find_matches_whole_components_only() {
        let root = tree();
        assert!(
            find(
                Path::new("/home/tobi"),
                &root,
                Path::new("/home/tobi/.cache"),
                NATIVE
            )
            .is_some()
        );
        assert!(
            find(
                Path::new("/home/tobi"),
                &root,
                Path::new("/home/tobi/.cache/blob.bin"),
                NATIVE
            )
            .is_some()
        );
        assert!(
            find(
                Path::new("/home/tobi"),
                &root,
                Path::new("/home/tobi/cache"),
                NATIVE
            )
            .is_none()
        );
        assert!(
            find(
                Path::new("/elsewhere"),
                &root,
                Path::new("/home/tobi/notes.bin"),
                NATIVE
            )
            .is_none()
        );
    }

    #[test]
    fn display_path_shortens_the_home_prefix() {
        let home = Path::new("/home/tobi");
        let separator = std::path::MAIN_SEPARATOR;
        assert_eq!(
            display_path(&home.join(".cache").join("npm"), Some(home), NATIVE),
            format!("~{separator}.cache{separator}npm")
        );
        assert_eq!(display_path(home, Some(home), NATIVE), "~");
        assert_eq!(
            display_path(Path::new("/var/log"), Some(home), NATIVE),
            "/var/log"
        );
        assert_eq!(
            display_path(Path::new("/var/log"), None, NATIVE),
            "/var/log"
        );
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
        let mut root = Node::directory("home");
        root.children.push(file(r"a\b", 7));
        assert!(
            find(home, &root, Path::new(r"/home/tobi/a\b"), posix).is_some()
        );
        assert!(
            find(home, &root, Path::new("/home/tobi/a/b"), posix).is_none()
        );
        assert!(!is_hidden(Path::new(r"/srv/x\.hidden"), posix));
        assert!(is_hidden(Path::new("/srv/.cache"), posix));
    }

    /// Marks are keyed by the exact path text, so `/a/b\c` and `/a/b/c`
    /// stay two marks even where `PathBuf` would call them equal.
    #[test]
    fn marks_compare_the_exact_path() {
        let mut marks = Marks::default();
        marks.toggle(target(r"/srv/b\c", 1));
        assert!(!marks.contains(Path::new("/srv/b/c")));
        assert!(marks.toggle(target("/srv/b/c", 1)));
        assert_eq!(marks.len(), 2);
        marks.remove(Path::new("/srv/b/c"));
        assert!(marks.contains(Path::new(r"/srv/b\c")));
    }
}

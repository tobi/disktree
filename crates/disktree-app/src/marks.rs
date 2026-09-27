//! The set of paths marked for removal.
//!
//! Marks are keyed by absolute path, not by tree position: the tree is
//! re-scanned after a removal, and a mark must survive that (or be reported as
//! gone) rather than silently pointing at a different node.

use std::path::{Path, PathBuf};

use disktree_core::removal::Target;
use disktree_core::tree::{Metric, Node};
use rustc_hash::FxHashSet;

/// Marked paths, in the order they were marked.
#[derive(Debug, Default)]
pub struct Marks {
    items: Vec<Target>,
    index: FxHashSet<PathBuf>,
}

impl Marks {
    pub fn contains(&self, path: &Path) -> bool {
        self.index.contains(path)
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
            self.items.retain(|item| item.path != path);
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.index.clear();
    }

    /// Re-read sizes from a freshly scanned tree and drop marks whose path no
    /// longer exists, so the tally never claims space that is already gone.
    pub fn refresh(&mut self, root_path: &Path, root: &Node, metric: Metric) {
        let mut resolved = Vec::with_capacity(self.items.len());
        for item in &self.items {
            match find(root_path, root, &item.path) {
                Some(node) => resolved.push(Target {
                    bytes: node.value(metric),
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
}

/// Walk the tree to the node at an absolute path. Path components are compared
/// one at a time, so a name containing a path separator cannot confuse it.
pub fn find<'a>(
    root_path: &Path,
    root: &'a Node,
    path: &Path,
) -> Option<&'a Node> {
    let relative = path.strip_prefix(root_path).ok()?;
    let mut node = root;
    for component in relative.components() {
        let name = component.as_os_str().to_string_lossy();
        node = node
            .children
            .iter()
            .find(|child| child.name.as_ref() == name)?;
    }
    Some(node)
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

        marks.refresh(root_path, &root, Metric::Bytes);
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
                Path::new("/home/tobi/.cache")
            )
            .is_some()
        );
        assert!(
            find(
                Path::new("/home/tobi"),
                &root,
                Path::new("/home/tobi/.cache/blob.bin")
            )
            .is_some()
        );
        assert!(
            find(
                Path::new("/home/tobi"),
                &root,
                Path::new("/home/tobi/cache")
            )
            .is_none()
        );
        assert!(
            find(
                Path::new("/elsewhere"),
                &root,
                Path::new("/home/tobi/notes.bin")
            )
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
}

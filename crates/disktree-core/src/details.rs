//! What an item is beyond its size.
//!
//! Found from the scanned tree and a few `lstat`s, without running git,
//! cheap enough for every pointer move. What takes longer is read later;
//! see [`crate::checkout`].

use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::classify::is_git_store;
use crate::tree::{Node, NodeKind};

/// What kind of checkout a directory is, from its `.git` alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckoutKind {
    Repository,
    /// Its branches, commits and stashes live in `repository`, not in the
    /// folder, so removing the folder leaves them there.
    Worktree {
        repository: PathBuf,
        locked: bool,
    },
    Submodule,
    /// A worktree whose repository has gone or moved; git cannot read it.
    Orphaned,
    Bare,
}

/// A checkout, or a folder holding nothing but one, such as a `dev tree`'s
/// `trees/<name>` around its `src`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutItem {
    /// What gets marked.
    pub item: PathBuf,
    /// Where git runs.
    pub at: PathBuf,
    pub kind: CheckoutKind,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Detail {
    Checkout(CheckoutItem),
    /// Linked worktrees of one repository side by side, largest first.
    Worktrees {
        repository: PathBuf,
        items: Vec<CheckoutItem>,
    },
}

/// Worktrees listed at most, largest first.
pub const WORKTREE_LIST_LIMIT: usize = 60;

pub fn details(node: &Node, path: &Path) -> Vec<Detail> {
    if let Some(item) = checkout_item(node, path) {
        return vec![Detail::Checkout(item)];
    }
    if !node.is_dir() {
        return Vec::new();
    }
    let mut groups: Vec<(PathBuf, Vec<CheckoutItem>)> = Vec::new();
    for child in node.children.iter().filter(|child| child.is_dir()) {
        let Some(item) = checkout_item(child, &path.join(&*child.name)) else {
            continue;
        };
        let CheckoutKind::Worktree { repository, .. } = &item.kind else {
            continue;
        };
        match groups.iter_mut().find(|(known, _)| known == repository) {
            Some((_, items)) => items.push(item),
            None => groups.push((repository.clone(), vec![item])),
        }
    }
    groups
        .into_iter()
        .filter(|(_, items)| items.len() >= 2)
        .map(|(repository, mut items)| {
            items.sort_by_key(|item| std::cmp::Reverse(item.bytes));
            items.truncate(WORKTREE_LIST_LIMIT);
            Detail::Worktrees { repository, items }
        })
        .collect()
}

pub fn checkout_item(node: &Node, path: &Path) -> Option<CheckoutItem> {
    if let Some(kind) = checkout_kind(path, Some(node)) {
        return Some(CheckoutItem {
            item: path.to_path_buf(),
            at: path.to_path_buf(),
            kind,
            bytes: node.bytes,
        });
    }
    let mut directories = node.children.iter().filter(|child| child.is_dir());
    let (Some(inner), None) = (directories.next(), directories.next()) else {
        return None;
    };
    let at = path.join(&*inner.name);
    let kind = checkout_kind(&at, Some(inner))?;
    Some(CheckoutItem {
        item: path.to_path_buf(),
        at,
        kind,
        bytes: node.bytes,
    })
}

/// From `.git` alone, without running git. The scanned tree answers when it
/// holds dot-files; a scan without them falls back to the disk.
pub fn checkout_kind(path: &Path, node: Option<&Node>) -> Option<CheckoutKind> {
    let dot_git = path.join(".git");
    let kind = node
        .and_then(|node| node.child_named(".git"))
        .map(|child| child.kind)
        .or_else(|| {
            fs::symlink_metadata(&dot_git).ok().map(|metadata| {
                if metadata.is_dir() {
                    NodeKind::Directory
                } else {
                    NodeKind::File
                }
            })
        });
    match kind {
        Some(NodeKind::Directory) => Some(CheckoutKind::Repository),
        Some(NodeKind::File) => linked_kind(&dot_git, path),
        _ => {
            let node = node?;
            let inside_git =
                path.components().any(|part| part.as_os_str() == ".git");
            (!inside_git && is_git_store(node)).then_some(CheckoutKind::Bare)
        }
    }
}

/// A `.git` file names the checkout's own git directory: a worktree's sits
/// in its repository's `worktrees/` beside a `commondir` file, a
/// submodule's in the superproject's `modules/`.
fn linked_kind(dot_git: &Path, checkout: &Path) -> Option<CheckoutKind> {
    let text = fs::read_to_string(dot_git).ok()?;
    let target = text.strip_prefix("gitdir:")?.trim();
    let git_directory = normalize(&checkout.join(target));
    if !exists(&git_directory) {
        return Some(CheckoutKind::Orphaned);
    }
    if git_directory
        .components()
        .any(|part| part.as_os_str() == "modules")
    {
        return Some(CheckoutKind::Submodule);
    }
    let Ok(common) = fs::read_to_string(git_directory.join("commondir")) else {
        return Some(CheckoutKind::Repository);
    };
    let common = normalize(&git_directory.join(common.trim()));
    if !exists(&common) {
        return Some(CheckoutKind::Orphaned);
    }
    let repository = if common.file_name().is_some_and(|name| name == ".git") {
        common
            .parent()
            .map_or_else(|| common.clone(), Path::to_path_buf)
    } else {
        common
    };
    Some(CheckoutKind::Worktree {
        locked: exists(&git_directory.join("locked")),
        repository,
    })
}

/// `..` and `.` resolved without asking the disk, so a path git wrote
/// relative to one directory compares equal to the same path written from
/// another.
pub fn normalize(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normal.pop() {
                    normal.push(part);
                }
            }
            other => normal.push(other),
        }
    }
    normal
}

pub fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_normalized_without_the_disk() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(normalize(Path::new("a/../../b")), PathBuf::from("../b"));
    }

    #[test]
    fn a_git_file_that_names_nothing_is_an_orphaned_worktree() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tree = temp.path().join("tree");
        fs::create_dir(&tree).expect("mkdir");
        fs::write(tree.join(".git"), "gitdir: ../gone/worktrees/tree\n")
            .expect("write");
        assert_eq!(checkout_kind(&tree, None), Some(CheckoutKind::Orphaned));
        assert_eq!(checkout_kind(temp.path(), None), None);
    }
}

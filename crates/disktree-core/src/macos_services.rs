//! Read-only visibility into macOS's private, on-volume service stores.
//!
//! These figures come from the same tree as the treemap, not another walk or
//! a subtraction from APFS capacity. They therefore share hardlink accounting
//! and never add volume or snapshot usage to file totals.

use std::path::{Path, PathBuf};

use crate::scan::ScanOptions;
use crate::tree::Node;

/// A service store whose name alone does not explain its contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Fsevents,
    Spotlight,
}

impl Service {
    pub const ALL: [Self; 2] = [Self::Fsevents, Self::Spotlight];

    pub const fn name(self) -> &'static str {
        match self {
            Self::Fsevents => ".fseventsd",
            Self::Spotlight => ".Spotlight-V100",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Fsevents => "FSEvents history",
            Self::Spotlight => "Spotlight index",
        }
    }

    pub const fn guidance(self) -> &'static str {
        match self {
            Self::Fsevents => {
                "Filesystem change history used by backup and sync apps. \
                 At an administrator's explicit request, Apple's root-only \
                 purge API can shorten old history. Other consumers may need \
                 a full rescan. disktree does not purge it."
            }
            Self::Spotlight => {
                "Search metadata for this volume. Keep indexing for file \
                 search, or choose folders in Spotlight's Search Privacy \
                 settings. Turning indexing off reduces search coverage; \
                 removing an index alone can cause it to be rebuilt. \
                 disktree does not change indexing."
            }
        }
    }

    pub const fn documentation(self) -> &'static str {
        match self {
            Self::Fsevents => {
                "https://developer.apple.com/library/archive/documentation/\
                 Darwin/Conceptual/FSEvents_ProgGuide/UsingtheFSEventsFramework/\
                 UsingtheFSEventsFramework.html"
            }
            Self::Spotlight => {
                "https://support.apple.com/guide/mac-help/\
                 prevent-spotlight-searches-in-files-mchlp2811/mac"
            }
        }
    }
}

pub fn is_service_name(name: &str) -> bool {
    Service::ALL
        .iter()
        .any(|service| name.eq_ignore_ascii_case(service.name()))
}

/// Unknown contents never acquire a zero-byte measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Usage {
    Measured(u64),
    /// Bytes successfully measured, with some contents still unknown.
    Incomplete(u64),
    Unavailable,
    OutsideScan,
}

/// Service-store location for whole startup-disk and external-volume scans.
/// A home scan has no service-store figures; it is not evidence of absence.
pub fn stores_volume(root: &Path) -> Option<PathBuf> {
    let data = Path::new(crate::space::MACOS_DATA_VOLUME);
    if data.starts_with(root) || root == data {
        return Some(data.to_path_buf());
    }
    if let Ok(below) = root.strip_prefix("/Volumes") {
        return below
            .components()
            .next()
            .map(|name| Path::new("/Volumes").join(name.as_os_str()));
    }
    if root
        .file_name()
        .is_some_and(|name| is_service_name(&name.to_string_lossy()))
    {
        return root.parent().map(Path::to_path_buf);
    }
    None
}

/// Look up an actual path in the scan; no I/O and no invented totals.
pub fn usage(
    root_path: &Path,
    tree: &Node,
    options: &ScanOptions,
    path: &Path,
) -> Usage {
    let Ok(below) = path.strip_prefix(root_path) else {
        return Usage::OutsideScan;
    };
    let mut node = tree;
    for part in below.components() {
        let Some(child) = node.child_named(&part.as_os_str().to_string_lossy())
        else {
            // Missing from a walk can mean failed metadata, filtering, or
            // no directory. It cannot establish a service's installation.
            return Usage::Unavailable;
        };
        node = child;
    }
    if !node.is_dir() {
        return Usage::Unavailable;
    }
    if node.read_error || !options.include_hidden || options.max_depth.is_some()
    {
        return if node.bytes == 0 {
            Usage::Unavailable
        } else {
            Usage::Incomplete(node.bytes)
        };
    }
    Usage::Measured(node.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Metric, NodeKind, aggregate};

    #[test]
    fn unavailable_is_not_zero_and_descendant_failures_are_incomplete() {
        let path = Path::new("/volume/.Spotlight-V100");
        let mut tree = Node::directory("volume");
        let options = ScanOptions::default();
        assert_eq!(
            usage(Path::new("/volume"), &tree, &options, path),
            Usage::Unavailable
        );
        let mut store = Node::directory(".Spotlight-V100");
        store.read_error = true;
        tree.children.push(store);
        aggregate(&mut tree, Metric::Bytes);
        assert_eq!(
            usage(Path::new("/volume"), &tree, &options, path),
            Usage::Unavailable
        );
        let store = &mut tree.children[0];
        store
            .children
            .push(Node::entry("index", NodeKind::File, 4096));
        let mut locked = Node::directory("locked");
        locked.read_error = true;
        store.read_error = false;
        store.children.push(locked);
        aggregate(&mut tree, Metric::Bytes);
        assert_eq!(
            usage(Path::new("/volume"), &tree, &options, path),
            Usage::Incomplete(4096)
        );
    }

    #[test]
    fn readable_empty_store_is_measured_but_filtered_contents_are_unknown() {
        let tree = Node::directory(".fseventsd");
        let path = Path::new("/volume/.fseventsd");
        let mut options = ScanOptions::default();
        assert_eq!(usage(path, &tree, &options, path), Usage::Measured(0));
        options.include_hidden = false;
        assert_eq!(usage(path, &tree, &options, path), Usage::Unavailable);
        options.include_hidden = true;
        options.max_depth = Some(0);
        assert_eq!(usage(path, &tree, &options, path), Usage::Unavailable);
        assert_eq!(
            usage(Path::new("/Users/me"), &tree, &options, path),
            Usage::OutsideScan
        );
    }

    #[test]
    fn private_stores_belong_to_the_data_volume_and_external_volumes() {
        assert_eq!(
            stores_volume(Path::new("/")),
            Some(PathBuf::from(crate::space::MACOS_DATA_VOLUME))
        );
        assert_eq!(
            stores_volume(Path::new("/Volumes/Backup")),
            Some(PathBuf::from("/Volumes/Backup"))
        );
        assert_eq!(stores_volume(Path::new("/Users/me")), None);
    }
}

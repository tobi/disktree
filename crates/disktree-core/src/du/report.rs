//! The serial half of du: visit what the walk found in `fts` order and apply
//! GNU du's `process_file` to each entry, printing as it goes.
//!
//! GNU accumulates with a stack of per-level sums that it pushes and pops as
//! `fts` changes depth. Here each directory's visit returns its own size and
//! the sums of what is below it, which is the same arithmetic without the
//! bookkeeping: `ent` holds the sizes of the entries directly inside a
//! directory, `subdir` the totals of the directories below those.

use rustc_hash::FxHashSet;

use super::args::{Options, TimeKind};
use super::walk::{
    Entry, ListError, Listing, Meta, Root, StatError, Time, join,
};

/// `struct duinfo`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dui {
    pub size: u64,
    pub inodes: u64,
    /// The latest of the chosen timestamp, `None` before any was seen.
    pub tmax: Option<Time>,
    /// The latest modification, whatever `--time` chose: the JSON output
    /// reports it as when the entry was last written.
    pub mtime_max: Option<Time>,
}

impl Dui {
    fn add(&mut self, other: &Self) {
        // GNU saturates the byte count and prints "Infinity" at the top.
        self.size = self.size.saturating_add(other.size);
        self.inodes = self.inodes.wrapping_add(other.inodes);
        self.tmax = self.tmax.max(other.tmax);
        self.mtime_max = self.mtime_max.max(other.mtime_max);
    }
}

/// The kind of thing an entry is, for the JSON output. Computed only when
/// that output was asked for.
#[derive(Clone, Copy, Debug, Default)]
pub struct Kind {
    pub category: crate::classify::Category,
    pub reclaim: Option<crate::classify::Reclaim>,
    /// Whether `reclaim` was decided by this entry's own name rather than
    /// inherited, which is where a clean command applies.
    pub reclaim_here: bool,
}

/// One line of du's output, with what the JSON output adds to it.
#[derive(Debug)]
pub struct Line<'a> {
    pub path: &'a [u8],
    pub dui: Dui,
    pub meta: Option<&'a Meta>,
    pub kind: Kind,
}

pub trait Sink {
    fn line(&mut self, line: &Line<'_>);
    fn total(&mut self, dui: &Dui);
    /// Called before anything goes to standard error, so that the two
    /// streams interleave as GNU's do when they share a terminal.
    fn flush(&mut self);
}

pub struct Report<'a, S: Sink> {
    options: &'a Options,
    program: &'a str,
    sink: S,
    seen: FxHashSet<(u64, u64)>,
    ancestors: Vec<(u64, u64)>,
    total: Dui,
    ok: bool,
}

struct Contribution {
    own: Dui,
    own_counts_in_parent: bool,
    below: Dui,
}

impl<'a, S: Sink> Report<'a, S> {
    pub fn new(options: &'a Options, program: &'a str, sink: S) -> Self {
        Self {
            options,
            program,
            sink,
            seen: FxHashSet::default(),
            ancestors: Vec::new(),
            total: Dui::default(),
            ok: true,
        }
    }

    pub fn error(&mut self, message: &str) {
        self.sink.flush();
        eprintln!("{}: {message}", self.program);
        self.ok = false;
    }

    /// Report one operand the walk has finished with.
    pub fn operand(&mut self, root: &Root) {
        let root_dev = root.meta.map_or(0, |meta| meta.dev);
        let kind = if self.options.json {
            root_kind(&root.path)
        } else {
            Kind::default()
        };
        self.visit(
            &root.path,
            &root.meta,
            root.dir.as_deref().and_then(std::sync::OnceLock::get),
            0,
            root_dev,
            kind,
        );
        self.ancestors.clear();
    }

    /// Print the grand total, if asked, and say whether all went well.
    pub fn finish(mut self) -> (bool, S) {
        if self.options.total {
            self.sink.total(&self.total);
        }
        self.sink.flush();
        (self.ok, self.sink)
    }

    fn dui(&self, meta: &Meta) -> Dui {
        let size = if self.options.apparent {
            // `usable_st_size`: only regular files and symbolic links have
            // an apparent size; directories count as nothing.
            let file_type = meta.file_type();
            if file_type == rustix::fs::FileType::RegularFile
                || file_type == rustix::fs::FileType::Symlink
            {
                u64::try_from(meta.size).unwrap_or(0)
            } else {
                0
            }
        } else {
            meta.blocks.saturating_mul(512)
        };
        let time = match self.options.time.as_ref().map(|(kind, _)| *kind) {
            Some(TimeKind::Accessed) => meta.atime,
            Some(TimeKind::Changed) => meta.ctime,
            Some(TimeKind::Modified) | None => meta.mtime,
        };
        Dui {
            size,
            inodes: 1,
            tmax: Some(time),
            mtime_max: Some(meta.mtime),
        }
    }

    fn print(&mut self, path: &[u8], dui: Dui, meta: &Meta, kind: Kind) {
        let value = if self.options.inodes {
            dui.inodes
        } else {
            dui.size
        };
        let threshold = self.options.threshold;
        let shown = if threshold < 0 {
            value <= threshold.unsigned_abs()
        } else {
            value >= threshold.unsigned_abs()
        };
        if shown {
            self.sink.line(&Line {
                path,
                dui,
                meta: Some(meta),
                kind,
            });
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one frame of fts's walk: the entry and where it sits"
    )]
    fn visit(
        &mut self,
        path: &[u8],
        meta: &Result<Meta, StatError>,
        listing: Option<&Listing>,
        level: i64,
        root_dev: u64,
        kind: Kind,
    ) -> Option<Contribution> {
        let options = self.options;
        let mut excluded = options.excludes.excludes(path);
        let meta = match meta {
            Ok(meta) => meta,
            Err(error) => {
                if !excluded {
                    let message = match error {
                        StatError::Dangling => {
                            format!(
                                "cannot access {}",
                                super::quote::always(path)
                            )
                        }
                        StatError::Errno(errno) => format!(
                            "cannot access {}: {}",
                            super::quote::always(path),
                            super::errno_text(*errno)
                        ),
                    };
                    self.error(&message);
                }
                return None;
            }
        };
        // `-x` can only exclude what is below an operand.
        if options.one_file_system && level > 0 && meta.dev != root_dev {
            excluded = true;
        }
        if excluded {
            return None;
        }
        if !options.count_links
            && (options.hash_all || (!meta.is_dir() && meta.nlink > 1))
            && !self.seen.insert(meta.key())
        {
            return None;
        }
        // A directory that is its own ancestor: `fts` reports a cycle
        // (`FTS_DC`) and du skips it without counting it. Only reachable
        // through `-L` with `-l`, where nothing is hashed, or a bind mount,
        // which GNU recognises as a mount point and skips silently too.
        if meta.is_dir() && self.ancestors.contains(&meta.key()) {
            return None;
        }
        let own = self.dui(meta);
        self.total.add(&own);

        if !meta.is_dir() {
            if (options.all && level <= options.max_depth) || level == 0 {
                self.print(path, own, meta, kind);
            }
            return Some(Contribution {
                own,
                own_counts_in_parent: true,
                below: Dui::default(),
            });
        }

        let mut ent = Dui::default();
        let mut subdir = Dui::default();
        let mut dir_type = true;
        match listing.and_then(|listing| listing.error) {
            Some(ListError::Unreadable(errno)) => {
                self.error(&format!(
                    "cannot read directory {}: {}",
                    super::quote::always(path),
                    super::errno_text(errno)
                ));
            }
            error => {
                self.ancestors.push(meta.key());
                let entries: &[Entry] =
                    listing.map_or(&[], |listing| &listing.entries);
                for entry in entries {
                    let child_path = join(path, &entry.name);
                    let child_kind = if options.json {
                        child_kind(kind, entry, entries)
                    } else {
                        Kind::default()
                    };
                    let child = self.visit(
                        &child_path,
                        &entry.meta,
                        entry.dir.as_deref().and_then(std::sync::OnceLock::get),
                        level + 1,
                        root_dev,
                        child_kind,
                    );
                    if let Some(child) = child {
                        if child.own_counts_in_parent {
                            ent.add(&child.own);
                        }
                        subdir.add(&child.below);
                    }
                }
                self.ancestors.pop();
                if let Some(ListError::Partial(errno)) = error {
                    // `FTS_ERR` at the postorder visit: not a directory
                    // type as far as printing goes.
                    dir_type = false;
                    self.error(&format!(
                        "{}: {}",
                        super::quote::when_needed(path),
                        super::errno_text(errno)
                    ));
                }
            }
        }

        let mut to_print = own;
        to_print.add(&ent);
        if !options.separate_dirs {
            to_print.add(&subdir);
        }
        let depth_ok = level <= options.max_depth;
        if ((dir_type || options.all) && depth_ok) || level == 0 {
            self.print(path, to_print, meta, kind);
        }
        let mut below = ent;
        below.add(&subdir);
        Some(Contribution {
            own,
            own_counts_in_parent: !(options.separate_dirs && dir_type),
            below,
        })
    }
}

fn root_kind(path: &[u8]) -> Kind {
    use crate::classify::{Category, category_of_name, reclaim_of};
    let trimmed = path.strip_suffix(b"/").unwrap_or(path);
    let (parent, name) = match trimmed.iter().rposition(|&b| b == b'/') {
        Some(at) => (&trimmed[..at.max(1)], &trimmed[at + 1..]),
        None => (b".".as_slice(), trimmed),
    };
    let name = String::from_utf8_lossy(name);
    let has_sibling = |sibling: &str| {
        rustix::fs::statat(
            rustix::fs::CWD,
            join(parent, sibling.as_bytes()).as_slice(),
            rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
        )
        .is_ok()
    };
    let reclaim = reclaim_of(&name, Category::Other, has_sibling);
    Kind {
        category: category_of_name(&name).unwrap_or_default(),
        reclaim,
        reclaim_here: reclaim.is_some(),
    }
}

fn child_kind(parent: Kind, entry: &Entry, siblings: &[Entry]) -> Kind {
    use crate::classify::{category_of_name, reclaim_of};
    let is_dir = entry.meta.is_ok_and(|meta| meta.is_dir());
    if !is_dir {
        return Kind {
            reclaim_here: false,
            ..parent
        };
    }
    let name = String::from_utf8_lossy(&entry.name);
    let category = category_of_name(&name).unwrap_or(parent.category);
    if parent.reclaim.is_some() {
        return Kind {
            category,
            reclaim: parent.reclaim,
            reclaim_here: false,
        };
    }
    let has_sibling = |sibling: &str| {
        siblings
            .iter()
            .any(|other| &*other.name == sibling.as_bytes())
    };
    let reclaim = reclaim_of(&name, parent.category, has_sibling);
    Kind {
        category,
        reclaim,
        reclaim_here: reclaim.is_some(),
    }
}

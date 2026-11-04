//! How a tree reads as text.
//!
//! An agent reads these answers as tokens, so each is a short, aligned
//! table with the number that matters first, rather than JSON that repeats
//! every key on every row. Sizes are binary units, as in the window. The
//! rows are [`Entry`] values, so a listing reads the same whether it came
//! from the window or from this process's own scan.

use std::fmt::Write as _;
use std::path::Path;

use disktree_core::size::{
    human_bytes, human_bytes_range, human_count, share, share_bar,
};
use disktree_link::{Clones, Entry, Found, Listing};

/// Width of the share bar on a row.
const BAR: usize = 8;

pub fn observed(delta: Option<i64>) -> String {
    delta.map_or_else(
        || "not measured".to_string(),
        |delta| {
            let sign = if delta < 0 { "-" } else { "+" };
            format!("{sign}{}", human_bytes(delta.unsigned_abs()))
        },
    )
}

/// `~/x` for a path under the home directory, else the path as it is.
pub fn display(path: &Path, home: Option<&Path>) -> String {
    match home {
        Some(home) if path == home => "~".to_string(),
        Some(home) => path.strip_prefix(home).map_or_else(
            |_| path.display().to_string(),
            |rest| format!("~/{}", rest.display()),
        ),
        None => path.display().to_string(),
    }
}

/// The removals since the window's scan, each three ways, then the total:
/// what to report after several batches, since the tree is not rescanned.
pub fn ledger(ledger: &disktree_link::RemovalLedger, now: i64) -> String {
    let private = |bytes: Option<u64>| {
        bytes.map_or_else(|| "unknown".into(), human_bytes)
    };
    let mut out = String::from(
        "Removed since the scan (the tree already leaves these out; \
         reported counts a clone in every place, private is what the scan \
         saw as theirs alone, observed is the volume's measured change):\n",
    );
    for (index, cohort) in ledger.cohorts.iter().enumerate() {
        let _ = writeln!(
            out,
            "  {}. {} \u{00b7} {} removed \u{00b7} {} failed \u{00b7} {} \
             reported \u{00b7} {} private \u{00b7} {} observed \u{00b7} {}",
            index + 1,
            cohort.mode,
            cohort.removed,
            cohort.failed,
            human_bytes(cohort.reported),
            private(cohort.private),
            observed(cohort.observed),
            age(now, cohort.finished),
        );
    }
    let _ = writeln!(
        out,
        "  Total: {} reported \u{00b7} {} private \u{00b7} {} observed, \
         from the first removal's start to the last one's end",
        human_bytes(ledger.reported),
        private(ledger.private),
        observed(ledger.observed),
    );
    out
}

/// How long ago `then` was, from `now`, in one unit: `3 d`, `2 mo`.
pub fn age(now: i64, then: i64) -> String {
    if then <= 0 {
        return "age unknown".to_string();
    }
    let seconds = (now - then).max(0);
    match seconds {
        0..3_600 => "within the hour".to_string(),
        3_600..86_400 => format!("{} h ago", seconds / 3_600),
        86_400..5_184_000 => format!("{} d ago", seconds / 86_400),
        5_184_000..63_072_000 => format!("{} mo ago", seconds / 2_592_000),
        _ => format!("{} y ago", seconds / 31_536_000),
    }
}

/// What an entry is, in one phrase after its name.
pub fn describe(entry: &Entry, now: i64) -> String {
    let mut parts = Vec::new();
    // First, since it changes what the rest means: the size is the scan's.
    if entry.removed {
        parts.push("removed since the scan".to_string());
    } else if entry.removed_bytes > 0 {
        parts.push(format!(
            "{} of it removed since the scan",
            human_bytes(entry.removed_bytes)
        ));
    }
    if entry.is_dir {
        parts.push(format!("{} files", human_count(entry.files)));
    }
    if let Some(split) = entry.clones.as_ref().and_then(clone_split) {
        parts.push(split);
    }
    if entry.category != "other" {
        parts.push(entry.category.clone());
    }
    if let Some(reclaim) = &entry.reclaim {
        parts.push(format!("cleanup candidate: {reclaim}"));
    }
    if entry.read_error {
        parts.push("unreadable".to_string());
    }
    parts.push(age(now, entry.modified));
    parts.join(" \u{00b7} ")
}

/// The split, then what removing the entry alone frees; `None` without
/// clone blocks.
fn clone_split(clones: &Clones) -> Option<String> {
    let split = clones.split()?;
    Some(format!(
        "{split} \u{00b7} frees \u{2248} {} alone{}",
        human_bytes_range(clones.lower, clones.upper),
        if clones.partial { " (partial)" } else { "" },
    ))
}

/// A line for a header: how much of `entry` is clone blocks, and how much
/// of it is listed twice. `None` without clone blocks, so reports off APFS
/// read as they always have.
pub fn clone_summary(entry: &Entry) -> Option<String> {
    let clones = entry.clones.filter(|clones| clones.shared > 0)?;
    let mut line = format!(
        "APFS clones: {} of this is shared clone blocks",
        human_bytes(clones.shared)
    );
    if clones.repeated > 0 {
        let _ = write!(
            line,
            "; \u{2248} {} is counted again, so the files here take \
             \u{2248} {} on disk",
            human_bytes(clones.repeated),
            human_bytes(entry.bytes.saturating_sub(clones.repeated)),
        );
    }
    let _ = writeln!(
        line,
        "{}. Rows split private from shared bytes; shared blocks stay while \
         any clone of them remains, inside this scan or outside it.",
        if clones.partial { " (partial)" } else { "" },
    );
    Some(line)
}

/// One row of a listing: share of `total`, size, name and description.
pub fn row(entry: &Entry, name: &str, total: u64, now: i64) -> String {
    let mut name = name.to_string();
    if entry.is_dir {
        name.push('/');
    }
    format!(
        "{} {:>3.0}%  {:>9}  {name}  {}",
        share_bar(entry.bytes, total, BAR),
        share(entry.bytes, total),
        human_bytes(entry.bytes),
        describe(entry, now),
    )
}

/// The children of a listing, largest first, with a line for the rest.
pub fn children(listing: &Listing, now: i64) -> String {
    let mut out = String::new();
    let total = listing.entry.bytes.max(1);
    for child in &listing.children {
        let _ = writeln!(out, "{}", row(child, &child.name, total, now));
    }
    if listing.more > 0 {
        let _ = writeln!(
            out,
            "{} {:>3.0}%  {:>9}  \u{2026} {} more",
            share_bar(listing.more_bytes, total, BAR),
            share(listing.more_bytes, total),
            human_bytes(listing.more_bytes),
            listing.more,
        );
    }
    if listing.children.is_empty() {
        out.push_str(if listing.entry.is_dir {
            "(empty, or not read)\n"
        } else {
            "(a file)\n"
        });
    }
    out
}

/// Search matches, each with its full path, sharing `total`.
pub fn matches(
    found: &[Found],
    total: u64,
    now: i64,
    home: Option<&Path>,
) -> String {
    let mut out = String::new();
    for item in found {
        let _ = writeln!(
            out,
            "{}",
            row(&item.entry, &display(&item.path, home), total, now)
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use disktree_core::tree::{Draft, Metric, NodeKind, Tree, aggregate};
    use std::path::PathBuf;

    #[test]
    fn paths_under_home_are_shortened() {
        let home = PathBuf::from("/home/ann");
        assert_eq!(display(&home, Some(&home)), "~");
        assert_eq!(display(&home.join("src"), Some(&home)), "~/src");
        assert_eq!(display(Path::new("/var"), Some(&home)), "/var");
        assert_eq!(display(Path::new("/var"), None), "/var");
    }

    #[test]
    fn ages_pick_one_unit() {
        let now = 1_800_000_000;
        assert_eq!(age(now, 0), "age unknown");
        assert_eq!(age(now, now - 10), "within the hour");
        assert_eq!(age(now, now - 7_200), "2 h ago");
        assert_eq!(age(now, now - 3 * 86_400), "3 d ago");
        assert_eq!(age(now, now - 70 * 86_400), "2 mo ago");
        assert_eq!(age(now, now - 800 * 86_400), "2 y ago");
    }

    fn entry(clones: Option<Clones>) -> Entry {
        Entry {
            name: "w".into(),
            is_dir: true,
            bytes: 12 * 1024 * 1024,
            files: 12,
            category: "other".into(),
            reclaim: None,
            modified: 0,
            read_error: false,
            clones,
            removed: false,
            removed_bytes: 0,
        }
    }

    #[test]
    fn removed_entries_say_so_first() {
        let gone = Entry {
            removed: true,
            ..entry(None)
        };
        assert_eq!(
            describe(&gone, 0),
            "removed since the scan \u{00b7} 12 files \u{00b7} age unknown"
        );
        let part = Entry {
            removed_bytes: 4 * 1024 * 1024,
            ..entry(None)
        };
        assert_eq!(
            describe(&part, 0),
            "4.0 MiB of it removed since the scan \u{00b7} 12 files \u{00b7} \
             age unknown"
        );
    }

    #[test]
    fn rows_without_clones_read_as_they_always_have() {
        let plain = entry(None);
        assert_eq!(describe(&plain, 0), "12 files \u{00b7} age unknown");
        assert_eq!(
            row(&plain, "w", plain.bytes, 0),
            "\u{2593}\u{2593}\u{2593}\u{2593}\u{2593}\u{2593}\u{2593}\
             \u{2593} 100%     12 MiB  w/  12 files \u{00b7} age unknown"
        );
        assert_eq!(clone_summary(&plain), None);
        // A modified clone that shares nothing any more says nothing new.
        let unshared = entry(Some(Clones {
            private: plain.bytes,
            shared: 0,
            repeated: 0,
            lower: plain.bytes,
            upper: plain.bytes,
            partial: false,
        }));
        assert_eq!(describe(&unshared, 0), describe(&plain, 0));
        assert_eq!(clone_summary(&unshared), None);
    }

    #[test]
    fn clone_rows_split_private_from_shared_and_say_what_goes_alone() {
        let full = entry(Some(Clones {
            private: 4096,
            shared: 12 * 1024 * 1024 - 4096,
            repeated: 0,
            lower: 4096,
            upper: 4096,
            partial: false,
        }));
        assert_eq!(
            describe(&full, 0),
            "12 files \u{00b7} 4.0 KiB private \u{00b7} 12 MiB shared with \
             other APFS clones \u{00b7} frees \u{2248} 4.0 KiB alone \u{00b7} \
             age unknown"
        );
        let partial = entry(Some(Clones {
            lower: 0,
            upper: 8 * 1024 * 1024,
            partial: true,
            ..full.clones.expect("clones")
        }));
        let text = describe(&partial, 0);
        assert!(
            text.contains("frees \u{2248} 0 B\u{2013}8.0 MiB alone (partial)"),
            "{text}"
        );
    }

    #[test]
    fn the_header_says_how_much_is_counted_again() {
        let mut root = entry(Some(Clones {
            private: 0,
            shared: 12 * 1024 * 1024,
            repeated: 8 * 1024 * 1024,
            lower: 4 * 1024 * 1024,
            upper: 4 * 1024 * 1024,
            partial: false,
        }));
        let line = clone_summary(&root).expect("a summary");
        assert!(line.starts_with("APFS clones: 12 MiB"), "{line}");
        assert!(line.contains("\u{2248} 8.0 MiB is counted again"), "{line}");
        assert!(line.contains("take \u{2248} 4.0 MiB on disk"), "{line}");
        assert!(line.contains("outside it"), "{line}");
        root.clones = root.clones.map(|clones| Clones {
            repeated: 0,
            ..clones
        });
        let line = clone_summary(&root).expect("a summary");
        assert!(!line.contains("counted again"), "{line}");
    }

    #[test]
    fn a_listing_is_largest_first_with_the_tail_summed() {
        let mut root = Draft::directory("r");
        for (name, bytes) in [("a", 10), ("b", 60), ("c", 30)] {
            root.children
                .push(Draft::entry(name, NodeKind::File, bytes));
        }
        let mut root = Tree::from(root);
        aggregate(&mut root, Metric::Bytes);
        let listing = Listing::of(
            Path::new("/r"),
            root.root().bytes,
            Path::new("/r"),
            root.root(),
            2,
        );
        let text = children(&listing, 0);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("60 B  b "), "{}", lines[0]);
        assert!(lines[1].contains("30 B  c "), "{}", lines[1]);
        assert!(lines[2].contains("1 more"), "{}", lines[2]);
        assert!(lines[2].contains("10 B"), "{}", lines[2]);
    }
}

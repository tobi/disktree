//! The removal's own list: every target as the run started, what the scan
//! said each one was, and what happened to it.
//!
//! Removing and Done both draw it, so the list a person approved stays on
//! screen while it is worked through and after, instead of only the names
//! of what already went. It is also the run's only record: the counts the
//! agent is told, the ledger's cohort and what the tree marks as gone are
//! all read from it, so they cannot disagree with what the screen shows.

use std::path::Path;

use disktree_core::classify::{Category, Reclaim};
use disktree_core::removal::Target;

/// What happened to one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Not reported yet: waiting, in hand, or never reached after a stop.
    Waiting,
    Removed,
    Failed(String),
}

/// One target of the run.
#[derive(Clone, Debug)]
pub struct Item {
    /// The plan's target, as the worker is given it: its path and the
    /// listed allocation when it was marked.
    pub target: Target,
    /// The scan's kind for it, for its colour.
    pub category: Category,
    /// Why the scan thinks its space can be had back, if it does.
    pub reclaim: Option<Reclaim>,
    /// What the scan saw as only this item's. `None` where it cannot say,
    /// as in an apparent-size scan.
    pub private: Option<u64>,
    pub status: Status,
}

impl Item {
    /// Why it is on the list, in the words the legend and "Worth a look"
    /// use: the reclaim reason when there is one, else its kind.
    pub fn kind(&self) -> Option<&'static str> {
        kind(self.category, self.reclaim)
    }
}

/// Why a path is listed, as Review and Removing both say it: the reclaim
/// reason when the scan has one, else its kind, else nothing.
pub fn kind(
    category: Category,
    reclaim: Option<Reclaim>,
) -> Option<&'static str> {
    reclaim
        .map(Reclaim::label)
        .or_else(|| (category != Category::Other).then(|| category.name()))
}

/// The run's items in the order the worker takes them.
#[derive(Clone, Debug, Default)]
pub struct RunList {
    pub items: Vec<Item>,
    /// Where the next report is expected: the worker goes in plan order.
    next: usize,
    /// Keep the item in hand in view, as the live log keeps its newest
    /// line: paused while the person has scrolled it out of view, and on
    /// again once it is back in view or they ask.
    pub follow: bool,
    /// The wheel moved the list since the last poll, which then decides
    /// whether that paused following.
    pub scrolled_by_hand: bool,
    /// Failures reported against no listed target. The worker reports
    /// every failure, a start that failed included, against a listed one;
    /// counted anyway, so the run never reads as clean when it was not.
    unlisted_failures: usize,
}

/// The run's figures so far, beside what the whole run would come to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    pub removed: usize,
    /// Listed items that failed, so `removed + failed` never exceeds
    /// `total` and the rest are the ones not reached.
    pub failed: usize,
    /// Failures reported against no listed item.
    pub unlisted: usize,
    pub total: usize,
    pub reported_done: u64,
    pub reported_all: u64,
    /// `None` once any item's private part is unknown.
    pub private_done: Option<u64>,
    pub private_all: Option<u64>,
}

impl Totals {
    /// Every failure the worker reported, listed or not: what the agent
    /// and the ledger are told, so no failure is lost to a stray path.
    pub const fn failed_all(&self) -> usize {
        self.failed + self.unlisted
    }
}

impl RunList {
    pub const fn new(items: Vec<Item>) -> Self {
        Self {
            items,
            next: 0,
            follow: true,
            scrolled_by_hand: false,
            unlisted_failures: 0,
        }
    }

    /// Record the worker's report on `path`. Returns the item's index, or
    /// `None` for a path it never listed, which still counts if it failed.
    pub fn settle(
        &mut self,
        path: &Path,
        outcome: &Result<(), String>,
    ) -> Option<usize> {
        let waiting = |item: &Item| {
            item.status == Status::Waiting && item.target.path == path
        };
        // Reports come in plan order, so the expected one is checked
        // first; the whole list only if the worker ever skips ahead.
        let index = self
            .items
            .get(self.next..)
            .and_then(|rest| rest.iter().position(waiting))
            .map(|offset| self.next + offset)
            .or_else(|| self.items.iter().position(waiting));
        let Some(index) = index else {
            if outcome.is_err() {
                self.unlisted_failures += 1;
            }
            return None;
        };
        self.items[index].status = match outcome {
            Ok(()) => Status::Removed,
            Err(error) => Status::Failed(error.clone()),
        };
        self.next = index + 1;
        Some(index)
    }

    /// The item the worker has in hand, while it runs: the first one not
    /// reported yet.
    pub fn current(&self) -> Option<usize> {
        let rest = self.items.get(self.next..)?;
        rest.iter()
            .position(|item| item.status == Status::Waiting)
            .map(|offset| self.next + offset)
    }

    /// The item to keep in view: the one in hand, or the last one
    /// reported once there is none.
    pub fn focus(&self) -> Option<usize> {
        self.current().or_else(|| self.next.checked_sub(1))
    }

    /// The targets that went, in plan order: what the tree shows as gone
    /// and the ledger's cohort counts. One pass, however long the list.
    pub fn removed(&self) -> impl Iterator<Item = &Target> {
        self.items
            .iter()
            .filter(|item| item.status == Status::Removed)
            .map(|item| &item.target)
    }

    pub fn totals(&self) -> Totals {
        let mut totals = Totals {
            unlisted: self.unlisted_failures,
            total: self.items.len(),
            private_done: Some(0),
            private_all: Some(0),
            ..Totals::default()
        };
        let add = |sum: Option<u64>, part: Option<u64>| {
            sum.zip(part).map(|(sum, part)| sum + part)
        };
        for item in &self.items {
            totals.reported_all += item.target.bytes;
            totals.private_all = add(totals.private_all, item.private);
            match item.status {
                Status::Removed => {
                    totals.removed += 1;
                    totals.reported_done += item.target.bytes;
                    totals.private_done =
                        add(totals.private_done, item.private);
                }
                Status::Failed(_) => totals.failed += 1,
                Status::Waiting => {}
            }
        }
        totals
    }
}

/// `text`, a path as a person reads it, in about `budget` characters:
/// whole names are dropped from its middle into one `…`, keeping the first
/// name after `~` or the root, which says where it lives, and as much of
/// the end as fits, which says what it is. A path that cannot be made to
/// fit comes back as short as it goes; the row clips the rest.
pub fn elide_middle(text: &str, separator: char, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_string();
    }
    let parts: Vec<&str> = text.split(separator).collect();
    // `~/code/…`, `/var/…` (an empty first part) or `C:\Users\…`: the
    // root and the name under it.
    let head = 2.min(parts.len());
    if parts.len() <= head + 1 {
        return text.to_string();
    }
    let sep = separator.to_string();
    let head_text = parts[..head].join(&sep);
    let width = |tail: &[&str]| {
        head_text.chars().count()
            + 3
            + tail.iter().map(|name| name.chars().count()).sum::<usize>()
            + tail.len().saturating_sub(1)
    };
    let mut start = parts.len() - 1;
    while start > head + 1 && width(&parts[start - 1..]) <= budget {
        start -= 1;
    }
    format!("{head_text}{sep}\u{2026}{sep}{}", parts[start..].join(&sep))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, bytes: u64, private: Option<u64>) -> Item {
        Item {
            target: Target {
                path: path.into(),
                bytes,
                is_dir: true,
                hidden: false,
            },
            category: Category::Other,
            reclaim: None,
            private,
            status: Status::Waiting,
        }
    }

    #[test]
    fn a_short_path_is_left_alone() {
        assert_eq!(elide_middle("~/code/app", '/', 40), "~/code/app");
    }

    #[test]
    fn the_middle_goes_first_and_whole_names_at_a_time() {
        let path = "~/code/clients/sample-co/apps/dashboard/packages/ui";
        assert_eq!(
            elide_middle(path, '/', 36),
            "~/code/\u{2026}/apps/dashboard/packages/ui"
        );
        assert_eq!(elide_middle(path, '/', 24), "~/code/\u{2026}/packages/ui");
        // The last name stays, even past the budget.
        assert_eq!(elide_middle(path, '/', 4), "~/code/\u{2026}/ui");
    }

    #[test]
    fn an_absolute_path_keeps_its_first_directory() {
        assert_eq!(
            elide_middle("/var/lib/docker/overlay2/3f9c/diff", '/', 25),
            "/var/\u{2026}/overlay2/3f9c/diff"
        );
    }

    #[test]
    fn a_windows_path_splits_on_its_own_separator() {
        assert_eq!(
            elide_middle(r"C:\Users\pat\AppData\Local\Temp\build", '\\', 22),
            "C:\\Users\\\u{2026}\\Temp\\build"
        );
    }

    #[test]
    fn nothing_to_drop_returns_the_path() {
        assert_eq!(
            elide_middle("~/a-very-long-directory-name", '/', 8),
            "~/a-very-long-directory-name"
        );
        assert_eq!(elide_middle("~/one/two", '/', 4), "~/one/two");
    }

    #[test]
    fn reports_settle_their_items_in_order() {
        let mut list = RunList::new(vec![
            item("/r/a", 10, Some(4)),
            item("/r/b", 20, Some(20)),
            item("/r/c", 30, Some(30)),
        ]);
        assert_eq!(list.current(), Some(0));
        assert_eq!(list.settle(Path::new("/r/a"), &Ok(())), Some(0));
        assert_eq!(list.current(), Some(1));
        assert_eq!(
            list.settle(Path::new("/r/b"), &Err("denied".into())),
            Some(1)
        );
        assert_eq!(list.current(), Some(2));
        let totals = list.totals();
        assert_eq!(totals.removed, 1);
        assert_eq!(totals.failed, 1);
        assert_eq!(totals.reported_done, 10);
        assert_eq!(totals.reported_all, 60);
        assert_eq!(totals.private_done, Some(4));
        assert_eq!(totals.private_all, Some(54));
        assert_eq!(list.items[1].status, Status::Failed("denied".into()));
        // A path it never listed settles no item, but a failure still
        // counts.
        assert_eq!(list.settle(Path::new(""), &Err("x".into())), None);
        assert_eq!(list.totals().failed, 1);
        assert_eq!(list.totals().failed_all(), 2);
        assert_eq!(list.settle(Path::new(""), &Ok(())), None);
        assert_eq!(list.totals().failed_all(), 2);
        assert_eq!(list.settle(Path::new("/r/c"), &Ok(())), Some(2));
        assert_eq!(list.current(), None);
        let removed: Vec<_> =
            list.removed().map(|target| target.path.clone()).collect();
        assert_eq!(removed, [Path::new("/r/a"), Path::new("/r/c")]);
    }

    #[test]
    fn one_unknown_private_part_makes_the_total_unknown() {
        let list = RunList::new(vec![
            item("/r/a", 10, Some(4)),
            item("/r/b", 20, None),
        ]);
        assert_eq!(list.totals().private_all, None);
        assert_eq!(list.totals().private_done, Some(0));
    }

    #[test]
    fn an_item_says_why_it_is_listed() {
        let mut build = item("/r/target", 1, None);
        build.reclaim = Some(Reclaim::BuildOutput);
        build.category = Category::Code;
        assert_eq!(build.kind(), Some("build output"));
        let mut media = item("/r/movies", 1, None);
        media.category = Category::Media;
        assert_eq!(media.kind(), Some("media"));
        assert_eq!(item("/r/x", 1, None).kind(), None);
    }
}

//! What each removal since the last scan freed, three ways.
//!
//! A removal no longer ends in a rescan, so several can follow one scan,
//! and each is a cohort here. *Reported* is the listed allocation of what
//! went, a clone counted in every place it appears. *Private* is the part
//! the scan saw as theirs alone. *Observed* is the volume's free space
//! after minus before, which other programs move too. The totals add the
//! first two; the third runs from the first cohort's start to the last
//! one's finish, so it is a measurement and never a sum of guesses.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use disktree_core::removal::{RemovalMode, Target};

/// A removal as it started: what it targets and the free space before.
#[derive(Clone, Debug)]
pub struct Start {
    pub targets: Vec<Target>,
    pub mode: RemovalMode,
    /// Bytes available on the scanned volume, measured as it started.
    pub before: Option<u64>,
    pub started: SystemTime,
}

/// One finished removal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cohort {
    pub mode: RemovalMode,
    pub removed: u64,
    pub failed: usize,
    /// The listed bytes of what was removed.
    pub reported: u64,
    /// What the scan saw as only theirs: `None` when it could not say, as
    /// in an apparent-size scan.
    pub private: Option<u64>,
    pub before: Option<u64>,
    pub after: Option<u64>,
    pub started: SystemTime,
    pub finished: SystemTime,
}

impl Cohort {
    /// Free space gained, negative when something else wrote more.
    pub fn observed(&self) -> Option<i64> {
        observed(self.before, self.after)
    }
}

fn observed(before: Option<u64>, after: Option<u64>) -> Option<i64> {
    let (before, after) = (before?, after?);
    Some(after.cast_signed().saturating_sub(before.cast_signed()))
}

/// Every cohort since one scan.
#[derive(Clone, Debug, Default)]
pub struct Ledger {
    /// The scan the cohorts follow, as `Disktree::scan_epoch` numbers it.
    scan: u64,
    cohorts: Vec<Cohort>,
}

/// The cohorts added up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Totals {
    pub count: usize,
    pub removed: u64,
    pub reported: u64,
    /// `None` once any cohort's private part is unknown.
    pub private: Option<u64>,
    /// From the first cohort's start to the last one's finish.
    pub observed: Option<i64>,
    pub started: SystemTime,
    pub finished: SystemTime,
}

impl Ledger {
    /// A removal is starting after scan `scan`: a newer scan starts a new
    /// ledger, since its figures already include what went before.
    pub fn begin(&mut self, scan: u64) {
        if self.scan != scan {
            self.scan = scan;
            self.cohorts.clear();
        }
    }

    pub fn push(&mut self, cohort: Cohort) {
        self.cohorts.push(cohort);
    }

    /// Every cohort recorded, whatever has been scanned since: what the
    /// Done screen lists while a rescan it asked for runs.
    pub fn cohorts(&self) -> &[Cohort] {
        &self.cohorts
    }

    /// The cohorts since scan `scan`, which is all the tree on screen has
    /// not seen.
    pub fn since(&self, scan: u64) -> &[Cohort] {
        if self.scan == scan {
            &self.cohorts
        } else {
            &[]
        }
    }

    /// The cohorts added up: reported and private are sums, observed runs
    /// from the first cohort's start to the last one's finish. The paths
    /// each cohort removed are not kept: they are the user's, and the tree
    /// already dropped them.
    pub fn totals(cohorts: &[Cohort]) -> Option<Totals> {
        let (first, last) = (cohorts.first()?, cohorts.last()?);
        Some(Totals {
            count: cohorts.len(),
            removed: cohorts.iter().map(|cohort| cohort.removed).sum(),
            reported: cohorts.iter().map(|cohort| cohort.reported).sum(),
            private: cohorts.iter().map(|cohort| cohort.private).sum(),
            observed: observed(first.before, last.after),
            started: first.started,
            finished: last.finished,
        })
    }
}

/// The paths a run removed: its targets whose outcome was success. A set,
/// since an agent can mark tens of thousands of paths and this runs on
/// the UI thread as the removal finishes.
pub fn removed(
    start: &Start,
    log: &[(PathBuf, Result<(), String>)],
) -> Vec<Target> {
    let done: HashSet<&Path> = log
        .iter()
        .filter(|(_, outcome)| outcome.is_ok())
        .map(|(path, _)| path.as_path())
        .collect();
    start
        .targets
        .iter()
        .filter(|target| done.contains(target.path.as_path()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cohort(
        reported: u64,
        private: Option<u64>,
        before: u64,
        after: u64,
        at: u64,
    ) -> Cohort {
        Cohort {
            mode: RemovalMode::Permanent,
            removed: 1,
            failed: 0,
            reported,
            private,
            before: Some(before),
            after: Some(after),
            started: SystemTime::UNIX_EPOCH + Duration::from_secs(at),
            finished: SystemTime::UNIX_EPOCH + Duration::from_secs(at + 5),
        }
    }

    #[test]
    fn totals_add_estimates_and_measure_from_first_start_to_last_finish() {
        let mut ledger = Ledger::default();
        ledger.begin(1);
        ledger.push(cohort(20_000, Some(200), 1_000, 1_195, 10));
        // Something else wrote 400 bytes between the two removals.
        ledger.push(cohort(10_000, Some(10_000), 795, 10_595, 30));
        let totals = Ledger::totals(ledger.since(1)).expect("two cohorts");
        assert_eq!(totals.count, 2);
        assert_eq!(totals.reported, 30_000);
        assert_eq!(totals.private, Some(10_200));
        assert_eq!(totals.observed, Some(9_595), "measured, not summed");
        assert_eq!(
            totals.started,
            SystemTime::UNIX_EPOCH + Duration::from_secs(10)
        );
    }

    #[test]
    fn a_new_scan_starts_a_new_ledger() {
        let mut ledger = Ledger::default();
        ledger.begin(1);
        ledger.push(cohort(1, Some(1), 0, 1, 0));
        assert!(ledger.since(2).is_empty(), "scan 2 already saw it");
        assert_eq!(ledger.cohorts().len(), 1, "the Done screen still can");
        ledger.begin(2);
        assert!(ledger.cohorts().is_empty());
    }

    #[test]
    fn one_unknown_private_part_makes_the_total_unknown() {
        let cohorts = [cohort(5, Some(5), 0, 5, 0), cohort(5, None, 5, 9, 9)];
        let totals = Ledger::totals(&cohorts).expect("totals");
        assert_eq!(totals.private, None);
        assert_eq!(totals.observed, Some(9));
    }
}

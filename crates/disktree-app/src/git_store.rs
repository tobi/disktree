//! Which checkouts git is asked about, when, and how many at once.
//!
//! The side panel's Git figure costs a `git status`, which can walk a
//! whole worktree. The panel draws on every frame and the pointer crosses
//! dozens of checkouts a second, so nothing here runs on a frame's say-so:
//! a frame says what it would like ([`GitStore::want`]), and the store
//! decides what runs ([`GitStore::next_jobs`]).
//!
//! - **Kinds of ask.** A selection is a direct action and always gets in.
//!   A hover that dwells is an implied one, and a refresh of a stale answer
//!   is the least urgent; both spend a token from a small bucket.
//! - **At most [`MAX_RUNNING`] at once**, newest wish first within a kind:
//!   what the user is looking at now beats what they looked at before.
//! - **Deadlines.** A wish lapses when nobody repeats it for a while; one
//!   that reaches the head of the queue lapsed is dropped unrun.
//! - **Answers age.** A selected checkout is asked again once its answer is
//!   stale; a hover shows whatever answer there is.
//! - **Generations.** [`GitStore::clear`] drops the queue, cancels what is
//!   running and ignores whatever it still reports.
//!
//! The store keeps no clock and runs nothing: every call takes `now`, and
//! the window runs the jobs, so the tests drive it with invented times.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;

use crate::git::GitState;

/// Probes running at once. Two, so one slow checkout cannot hold up every
/// other answer until its time limit.
pub const MAX_RUNNING: usize = 2;

/// How long the pointer rests on a checkout before that counts as asking.
/// Long enough that sweeping across a mosaic asks nothing, short enough
/// that pausing to read a tile feels immediate.
pub const DWELL: Duration = Duration::from_millis(500);

/// How long one probe may take before its git is killed. A checkout this
/// slow to read is one whose answer says so, not one to wait on.
pub const PROBE_LIMIT: Duration = Duration::from_secs(5);

/// When an answer is old enough to ask again, for the selected checkout.
const FRESH_FOR: Duration = Duration::from_mins(1);
/// A failure is asked again much later: another time-out costs as much.
const FAILURE_FRESH_FOR: Duration = Duration::from_mins(10);

/// Tokens the bucket holds, so a few deliberate hovers in a row all count.
const BUCKET: u32 = 4;
/// One token comes back this often: at most 30 hovers or refreshes a
/// minute once the bucket is empty.
const REFILL: Duration = Duration::from_secs(2);

/// Runs one probe of the checkout at a path, stopping when the flag is
/// set. The window's is `git::probe` within [`PROBE_LIMIT`]; tests count
/// calls with their own.
pub type GitProbe = Arc<dyn Fn(&Path, &Arc<AtomicBool>) -> Probe + Send + Sync>;

/// What brought the ask, most urgent first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Ask {
    /// The selection: a key or a click put it there.
    Direct,
    /// A hover that stayed for [`DWELL`].
    Dwell,
    /// The selected checkout's answer went stale.
    Refresh,
}

impl Ask {
    /// Whether the wish came from the selection rather than the pointer.
    /// There is one of each at a time, so a newer wish from the same one
    /// makes an older wish for another checkout moot.
    const fn by_selection(self) -> bool {
        !matches!(self, Self::Dwell)
    }

    /// How long a wish lasts in the queue without being repeated. A
    /// frame repeats it while the tile is still the one shown, so this is
    /// how long after the user moves on that the wish lapses.
    const fn lasts(self) -> Duration {
        match self {
            Self::Direct => Duration::from_secs(3),
            Self::Dwell => Duration::from_millis(1500),
            Self::Refresh => Duration::from_secs(5),
        }
    }
}

/// What a probe found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Probe {
    Known(GitState),
    /// Git is missing or could not read the checkout.
    Unreadable,
    /// Git took longer than [`PROBE_LIMIT`] and was stopped.
    TimedOut,
}

impl Probe {
    /// The Git figure's value.
    pub fn summary(&self) -> String {
        match self {
            Self::Known(state) => state.summary(),
            Self::Unreadable => "not readable".to_string(),
            // Stopped at its limit: say so rather than leave "asking" up,
            // or call it clean.
            Self::TimedOut => "too large to check".to_string(),
        }
    }

    const fn fresh_for(&self) -> Duration {
        match self {
            Self::Known(_) => FRESH_FOR,
            Self::Unreadable | Self::TimedOut => FAILURE_FRESH_FOR,
        }
    }
}

/// What the panel shows for a checkout.
#[derive(Debug, PartialEq, Eq)]
pub enum Shown<'a> {
    /// The last answer, stale or not: a refresh replaces it when it lands.
    Answer(&'a Probe),
    /// Waiting for a probe, or for the pointer to settle.
    Asking,
}

/// What became of a wish.
#[derive(Debug, PartialEq, Eq)]
pub enum Wanted {
    /// A fresh enough answer is there; ask again at `stale_at`.
    Answered { stale_at: Instant },
    /// Queued or running.
    Pending,
    /// The bucket is empty; a token is back at `retry_at`.
    Refused { retry_at: Instant },
}

/// A probe for the window to run, off the UI thread.
#[derive(Clone, Debug)]
pub struct Job {
    pub path: PathBuf,
    /// Set when the store no longer wants the answer: the probe stops.
    pub cancel: Arc<AtomicBool>,
    generation: u64,
}

struct Waiting {
    path: PathBuf,
    ask: Ask,
    /// When a frame last wanted it: the newest goes first.
    wanted_at: Instant,
    deadline: Instant,
}

struct Answer {
    probe: Probe,
    at: Instant,
}

pub struct GitStore {
    answers: FxHashMap<PathBuf, Answer>,
    queue: Vec<Waiting>,
    running: FxHashMap<PathBuf, Arc<AtomicBool>>,
    bucket: Bucket,
    generation: u64,
    /// Wishes dropped at the head of the queue, for the tests.
    pub lapsed: usize,
}

impl GitStore {
    pub fn new(now: Instant) -> Self {
        Self {
            answers: FxHashMap::default(),
            queue: Vec::new(),
            running: FxHashMap::default(),
            bucket: Bucket::new(now),
            generation: 0,
            lapsed: 0,
        }
    }

    /// A frame would like to know about `path`.
    pub fn want(&mut self, path: &Path, ask: Ask, now: Instant) -> Wanted {
        // Arrowing through a row of checkouts selects each for a moment;
        // only where the selection stops is still wanted. What already
        // runs finishes and is remembered.
        self.queue.retain(|waiting| {
            waiting.path == path
                || waiting.ask.by_selection() != ask.by_selection()
        });
        if self.running.contains_key(path) {
            return Wanted::Pending;
        }
        let ask = match self.answers.get(path) {
            Some(answer) => {
                let stale_at = answer.at + answer.probe.fresh_for();
                // A hover shows what is known; only the selection refreshes.
                if now < stale_at || ask != Ask::Direct {
                    return Wanted::Answered { stale_at };
                }
                Ask::Refresh
            }
            None => ask,
        };
        if let Some(waiting) =
            self.queue.iter_mut().find(|waiting| waiting.path == path)
        {
            waiting.ask = waiting.ask.min(ask);
            waiting.wanted_at = now;
            waiting.deadline = waiting.deadline.max(now + ask.lasts());
            return Wanted::Pending;
        }
        // The selection is what the user asked for, so the bucket never
        // turns it away; it only meters what they did not ask for outright.
        if ask != Ask::Direct
            && let Err(retry_at) = self.bucket.take(now)
        {
            return Wanted::Refused { retry_at };
        }
        self.queue.push(Waiting {
            path: path.to_path_buf(),
            ask,
            wanted_at: now,
            deadline: now + ask.lasts(),
        });
        Wanted::Pending
    }

    /// The probes to start now, within [`MAX_RUNNING`].
    pub fn next_jobs(&mut self, now: Instant) -> Vec<Job> {
        let mut jobs = Vec::new();
        while self.running.len() < MAX_RUNNING {
            let Some(head) = self
                .queue
                .iter()
                .enumerate()
                .min_by_key(|(_, waiting)| {
                    (waiting.ask, std::cmp::Reverse(waiting.wanted_at))
                })
                .map(|(index, _)| index)
            else {
                break;
            };
            let waiting = self.queue.swap_remove(head);
            if waiting.deadline <= now {
                self.lapsed += 1;
                continue;
            }
            let cancel = Arc::new(AtomicBool::new(false));
            self.running.insert(waiting.path.clone(), cancel.clone());
            jobs.push(Job {
                path: waiting.path,
                cancel,
                generation: self.generation,
            });
        }
        jobs
    }

    /// A job ended. False when its answer is no longer wanted.
    pub fn finish(&mut self, job: &Job, probe: Probe, now: Instant) -> bool {
        if job.generation != self.generation {
            return false;
        }
        self.running.remove(&job.path);
        self.answers
            .insert(job.path.clone(), Answer { probe, at: now });
        true
    }

    pub fn shown(&self, path: &Path) -> Shown<'_> {
        self.answers
            .get(path)
            .map_or(Shown::Asking, |answer| Shown::Answer(&answer.probe))
    }

    /// Forget everything: a new scan, or another machine. What is running
    /// is told to stop, and its answer is ignored when it comes.
    pub fn clear(&mut self) {
        self.cancel_running();
        self.generation += 1;
        self.answers.clear();
        self.queue.clear();
    }

    fn cancel_running(&mut self) {
        for (_, cancel) in self.running.drain() {
            cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// A closed window stops its probes rather than leave git running.
impl Drop for GitStore {
    fn drop(&mut self) {
        self.cancel_running();
    }
}

/// A token bucket kept as the time it is next full minus its capacity
/// (the generic cell rate algorithm): one instant instead of a count and a
/// clock, and no fractions.
struct Bucket {
    /// When the next token would be due if none were saved up.
    due: Instant,
}

impl Bucket {
    const fn new(now: Instant) -> Self {
        Self { due: now }
    }

    /// How far ahead of `due` a full bucket lets a take run.
    fn burst() -> Duration {
        REFILL * (BUCKET - 1)
    }

    /// Spend a token, or say when one is back.
    fn take(&mut self, now: Instant) -> Result<(), Instant> {
        if self.due > now + Self::burst() {
            return Err(self.due.checked_sub(Self::burst()).unwrap_or(now));
        }
        self.due = self.due.max(now) + REFILL;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean() -> Probe {
        Probe::Known(GitState {
            changed: Some(0),
            unpushed: Some(0),
            ..GitState::default()
        })
    }

    fn path(name: &str) -> PathBuf {
        PathBuf::from("/checkouts").join(name)
    }

    /// Take every slot with a probe of its own: each is selected and
    /// started before the next, as a selection that stays would be.
    fn fill_slots(store: &mut GitStore, now: Instant) -> Vec<Job> {
        (0..MAX_RUNNING)
            .flat_map(|n| {
                store.want(&path(&format!("busy-{n}")), Ask::Direct, now);
                store.next_jobs(now)
            })
            .collect()
    }

    #[test]
    fn a_selection_runs_once_and_then_reads_the_answer() {
        let t0 = Instant::now();
        let mut store = GitStore::new(t0);
        let alpha = path("alpha");
        assert_eq!(store.want(&alpha, Ask::Direct, t0), Wanted::Pending);
        let jobs = store.next_jobs(t0);
        assert_eq!(jobs.len(), 1);
        assert_eq!(store.shown(&alpha), Shown::Asking);

        // Every frame asks again while it runs; nothing more starts.
        for _ in 0..10 {
            assert_eq!(store.want(&alpha, Ask::Direct, t0), Wanted::Pending);
            assert!(store.next_jobs(t0).is_empty());
        }

        let done = t0 + Duration::from_millis(80);
        assert!(store.finish(&jobs[0], clean(), done));
        assert_eq!(store.shown(&alpha), Shown::Answer(&clean()));
        assert_eq!(
            store.want(&alpha, Ask::Direct, done),
            Wanted::Answered {
                stale_at: done + FRESH_FOR
            }
        );
        assert!(store.next_jobs(done).is_empty());
    }

    #[test]
    fn no_more_than_the_limit_run_at_once() {
        let t0 = Instant::now();
        let mut store = GitStore::new(t0);
        // A selection and a hover start; a third wish waits.
        store.want(&path("selected"), Ask::Direct, t0);
        store.want(&path("hovered"), Ask::Dwell, t0);
        let first = store.next_jobs(t0);
        assert_eq!(first.len(), MAX_RUNNING);
        store.want(&path("next"), Ask::Direct, t0);
        assert!(store.next_jobs(t0).is_empty(), "the slots are full");

        // One finishing frees one slot for the wish that waited.
        assert!(store.finish(&first[0], clean(), t0));
        let next = store.next_jobs(t0);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].path, path("next"));
    }

    #[test]
    fn a_selection_runs_before_a_hover() {
        let t0 = Instant::now();
        let mut store = GitStore::new(t0);
        let busy = fill_slots(&mut store, t0);

        let t1 = t0 + Duration::from_millis(100);
        store.want(&path("hovered"), Ask::Dwell, t1);
        let t2 = t1 + Duration::from_millis(100);
        store.want(&path("selected"), Ask::Direct, t2);

        assert!(store.finish(&busy[0], clean(), t2));
        assert_eq!(store.next_jobs(t2)[0].path, path("selected"));
        assert!(store.finish(&busy[1], clean(), t2));
        assert_eq!(store.next_jobs(t2)[0].path, path("hovered"));
    }

    #[test]
    fn arrowing_through_checkouts_runs_only_where_the_selection_stops() {
        let t0 = Instant::now();
        let mut store = GitStore::new(t0);
        let busy = fill_slots(&mut store, t0);
        let mut now = t0;
        for n in 0..10 {
            now += Duration::from_millis(30);
            store.want(&path(&format!("passed-{n}")), Ask::Direct, now);
        }
        // The pointer's wish is not the selection's to drop.
        store.want(&path("hovered"), Ask::Dwell, now);
        for job in &busy {
            assert!(store.finish(job, clean(), now));
        }
        let started: Vec<_> = store
            .next_jobs(now)
            .into_iter()
            .map(|job| job.path)
            .collect();
        assert_eq!(started, [path("passed-9"), path("hovered")]);
    }

    #[test]
    fn a_lapsed_wish_is_dropped_at_the_head_of_the_queue() {
        let t0 = Instant::now();
        let mut store = GitStore::new(t0);
        let slow = fill_slots(&mut store, t0);
        // The pointer rested here, then moved on and never came back.
        store.want(&path("passed"), Ask::Dwell, t0);

        let later = t0 + Ask::Dwell.lasts() + Duration::from_millis(1);
        assert!(store.finish(&slow[0], Probe::TimedOut, later));
        assert!(store.next_jobs(later).is_empty(), "nothing left to run");
        assert_eq!(store.lapsed, 1);
        assert_eq!(store.shown(&path("passed")), Shown::Asking);
    }

    #[test]
    fn a_wish_repeated_by_each_frame_does_not_lapse() {
        let t0 = Instant::now();
        let mut store = GitStore::new(t0);
        let slow = fill_slots(&mut store, t0);
        let selected = path("selected");
        let mut now = t0;
        for _ in 0..10 {
            now += Duration::from_secs(1);
            store.want(&selected, Ask::Direct, now);
        }
        assert!(store.finish(&slow[0], clean(), now));
        assert_eq!(store.next_jobs(now)[0].path, selected);
        assert_eq!(store.lapsed, 0);
    }

    #[test]
    fn hovers_spend_tokens_and_a_selection_does_not() {
        let t0 = Instant::now();
        let mut store = GitStore::new(t0);
        for n in 0..BUCKET {
            let wanted =
                store.want(&path(&format!("hover-{n}")), Ask::Dwell, t0);
            assert_eq!(wanted, Wanted::Pending);
        }
        let refused = store.want(&path("one-more"), Ask::Dwell, t0);
        assert_eq!(
            refused,
            Wanted::Refused {
                retry_at: t0 + REFILL
            }
        );
        assert_eq!(
            store.want(&path("selected"), Ask::Direct, t0),
            Wanted::Pending,
            "a selection is never turned away"
        );
        assert_eq!(
            store.want(&path("one-more"), Ask::Dwell, t0 + REFILL),
            Wanted::Pending,
            "a token is back"
        );
    }

    #[test]
    fn a_stale_answer_is_shown_and_refreshed_only_for_the_selection() {
        let t0 = Instant::now();
        let mut store = GitStore::new(t0);
        let alpha = path("alpha");
        store.want(&alpha, Ask::Direct, t0);
        let job = store.next_jobs(t0).remove(0);
        assert!(store.finish(&job, clean(), t0));

        let stale = t0 + FRESH_FOR;
        assert!(matches!(
            store.want(&alpha, Ask::Dwell, stale),
            Wanted::Answered { .. }
        ));
        assert!(store.next_jobs(stale).is_empty(), "a hover never refreshes");

        assert_eq!(store.want(&alpha, Ask::Direct, stale), Wanted::Pending);
        assert_eq!(store.next_jobs(stale).len(), 1);
        assert_eq!(
            store.shown(&alpha),
            Shown::Answer(&clean()),
            "the old answer stays up while the refresh runs"
        );
    }

    #[test]
    fn a_time_out_is_remembered_and_not_retried_soon() {
        let t0 = Instant::now();
        let mut store = GitStore::new(t0);
        let huge = path("huge");
        store.want(&huge, Ask::Direct, t0);
        let job = store.next_jobs(t0).remove(0);
        let done = t0 + PROBE_LIMIT;
        assert!(store.finish(&job, Probe::TimedOut, done));
        assert_eq!(store.shown(&huge), Shown::Answer(&Probe::TimedOut));
        let minute = done + FRESH_FOR;
        assert!(matches!(
            store.want(&huge, Ask::Direct, minute),
            Wanted::Answered { .. }
        ));
        assert!(store.next_jobs(minute).is_empty());
    }

    #[test]
    fn clearing_stops_what_runs_and_ignores_its_answer() {
        let t0 = Instant::now();
        let mut store = GitStore::new(t0);
        let jobs = fill_slots(&mut store, t0);
        assert_eq!(jobs.len(), MAX_RUNNING);
        store.want(&path("still-waiting"), Ask::Direct, t0);

        store.clear();
        assert!(jobs.iter().all(|job| job.cancel.load(Ordering::Relaxed)));
        assert!(!store.finish(&jobs[0], clean(), t0), "an old generation");
        assert_eq!(store.shown(&jobs[0].path), Shown::Asking);
        assert!(store.next_jobs(t0).is_empty(), "the queue went too");
    }

    #[test]
    fn the_figure_says_when_git_was_stopped() {
        assert_eq!(Probe::TimedOut.summary(), "too large to check");
        assert_eq!(Probe::Unreadable.summary(), "not readable");
        assert_eq!(clean().summary(), "clean, no stash");
    }

    #[test]
    fn dropping_the_store_stops_what_runs() {
        let t0 = Instant::now();
        let mut store = GitStore::new(t0);
        store.want(&path("alpha"), Ask::Direct, t0);
        let jobs = store.next_jobs(t0);
        drop(store);
        assert!(jobs[0].cancel.load(Ordering::Relaxed));
    }
}

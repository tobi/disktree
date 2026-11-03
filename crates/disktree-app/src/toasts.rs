//! Toasts in the main window: short notices that answer something the
//! person asked for, or offer the one thing worth acting on. Today that is
//! the updater. Design: "Update Notices" in the PwrDisk Claude Design
//! project, after PwrSnap's update banner and PwrRoute's toast.
//!
//! Two kinds of news, kept apart as PwrSnap does:
//!
//! - **The answer to Help › Check for Updates….** Only that menu item
//!   asks; the startup, timer and Settings checks never raise a toast
//!   (Settings answers its own button inline). While the check runs the
//!   card is live ("Checking for updates", then "Downloading update" with
//!   its percent) and has no countdown. Once it settles, an answer with
//!   nothing to install counts down [`ANSWER`] and goes, with a strip
//!   along its top that drains.
//! - **A release ready to install**, from any check: held for the slot in
//!   force (`updater::ready`). It is not pushed once: it is drawn from the
//!   held download whenever no live card or answer is in its place, so it
//!   comes back after an answer leaves. Later puts away that version only;
//!   a newer one, or asking again, brings it back. The title strip's chip
//!   offers it too, Later or not (`views::update_chip`).
//!
//! The timed answers sit in [`Toasts`], a short list with its own clock
//! that pauses while the pointer is over the stack, which is when someone
//! is reading a card or reaching for its button. The live and ready cards
//! come from [`UpdateWatch`], which keeps the updater's state as of its
//! last change, so a frame never asks the updater or the disk.
//!
//! The stack sits in the lower left of each screen's content, above its
//! footer, so it never covers the footer's buttons; dialogs draw over it.

use std::time::{Duration, Instant};

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    Animation, AnimationExt as _, AnyElement, App, Context, Div, ElementId,
    FontWeight, Hsla, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, relative,
};
use gpui_omarchy::{ActiveTheme as _, ButtonVariant, button};

use crate::semantics::AccessibleButton as _;
use crate::state::Disktree;
use crate::ui::{size, space, text};
use crate::update_channel::Slot;
use crate::updater::{self, Change, Held, Outcome, State};

/// The element id of the stack, for tests.
pub const STACK_ID: &str = "toast-stack";
/// The one key the updater's cards share, so a second check replaces the
/// first card rather than stacking another.
pub const UPDATE: &str = "update";
/// The most toasts on screen; an older one gives way to a newer one.
const MAX: usize = 3;
/// How long an answer stays: PwrSnap's and PwrRoute's 8 s.
pub const ANSWER: Duration = Duration::from_secs(8);

/// What colour a toast's title and edge take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// An answer, an offer, or something under way.
    Accent,
    /// Something went wrong.
    Danger,
}

/// What runs under a live card's sentence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Track {
    /// Work of unknown length: a sweep.
    Sweep,
    /// 0-100.
    Percent(u8),
}

/// A toast's buttons. Later is always the last one, so a click aimed at it
/// cannot land on Restart if the card changes under the pointer (PwrAgnt's
/// rule).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// The release's page on GitHub.
    ReleaseNotes(String),
    /// Where to download PwrDisk, for a copy that cannot update itself.
    DownloadPage,
    OpenLogs,
    CheckAgain,
    /// Install the held download and start again; `switch_back` for an
    /// older release. `restarting` once pressed.
    Restart {
        switch_back: bool,
        restarting: bool,
    },
    /// Put away the ready release until a newer one, or until asked.
    Later,
}

/// One notice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub key: &'static str,
    pub tone: Tone,
    pub title: String,
    pub body: String,
    /// A quieter line under the body.
    pub detail: Option<String>,
    /// A line in the danger tone: what went wrong with the last try.
    pub problem: Option<String>,
    pub actions: Vec<Action>,
    /// How long it stays once shown; `None` stays until put away.
    pub timeout: Option<Duration>,
    /// Work under way: no countdown, no close.
    pub track: Option<Track>,
}

impl Toast {
    fn new(tone: Tone, title: impl Into<String>, body: String) -> Self {
        Self {
            key: UPDATE,
            tone,
            title: title.into(),
            body,
            detail: None,
            problem: None,
            actions: Vec::new(),
            timeout: None,
            track: None,
        }
    }
}

#[derive(Clone, Debug)]
struct Entry {
    toast: Toast,
    remaining: Option<Duration>,
}

/// The timed toasts on screen, oldest first, and the clock their timeouts
/// run on.
#[derive(Clone, Debug, Default)]
pub struct Toasts {
    entries: Vec<Entry>,
    /// The pointer is over the stack: no timeout runs.
    hovered: bool,
    /// When the timeouts were last brought up to date.
    tick: Option<Instant>,
}

impl Toasts {
    /// Shows `toast`, in place of one with the same key.
    pub fn push(&mut self, toast: Toast, now: Instant) {
        self.settle(now);
        self.entries.retain(|entry| entry.toast.key != toast.key);
        let remaining = toast.timeout;
        self.entries.push(Entry { toast, remaining });
        let over = self.entries.len().saturating_sub(MAX);
        self.entries.drain(..over);
    }

    /// Takes away the toast with `key`; whether there was one.
    pub fn dismiss(&mut self, key: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.toast.key != key);
        self.entries.len() != before
    }

    /// Runs the timeouts up to `now` and drops what has run out; whether
    /// anything went.
    pub fn settle(&mut self, now: Instant) -> bool {
        let elapsed = self.elapsed(now);
        self.tick = Some(now);
        if self.hovered {
            return false;
        }
        let before = self.entries.len();
        self.entries.retain_mut(|entry| {
            entry.remaining = entry
                .remaining
                .map(|remaining| remaining.saturating_sub(elapsed));
            entry.remaining.is_none_or(|remaining| !remaining.is_zero())
        });
        self.entries.len() != before
    }

    fn elapsed(&self, now: Instant) -> Duration {
        self.tick
            .map_or(Duration::ZERO, |tick| now.saturating_duration_since(tick))
    }

    /// Pauses the timeouts while the pointer is over the stack.
    pub fn set_hovered(&mut self, hovered: bool, now: Instant) {
        self.settle(now);
        self.hovered = hovered;
    }

    /// A stack that is not drawn never hears the pointer leave: a click on
    /// × takes the card from under it, and a ready card can step aside for
    /// a removal or another channel. Left set, every later answer would
    /// wait for a hover that never ends, so [`stack`] calls this whenever
    /// it draws nothing.
    pub const fn forget_hover(&mut self) {
        self.hovered = false;
    }

    pub const fn is_hovered(&self) -> bool {
        self.hovered
    }

    /// How long until the next toast runs out, if one will.
    pub fn next_deadline(&self) -> Option<Duration> {
        if self.hovered {
            return None;
        }
        self.entries
            .iter()
            .filter_map(|entry| entry.remaining)
            .min()
    }

    /// How much of `key`'s time is left at `now`, 0 to 1, for its
    /// countdown strip; `None` for a toast that does not run out.
    pub fn left(&self, key: &str, now: Instant) -> Option<f32> {
        let entry = self.entries.iter().find(|entry| entry.toast.key == key)?;
        let total = entry.toast.timeout?;
        let mut remaining = entry.remaining?;
        if !self.hovered {
            remaining = remaining.saturating_sub(self.elapsed(now));
        }
        Some((remaining.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0))
    }

    pub fn get(&self, key: &str) -> Option<&Toast> {
        self.entries
            .iter()
            .find(|entry| entry.toast.key == key)
            .map(|entry| &entry.toast)
    }

    pub const fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Toast> {
        self.entries.iter().map(|entry| &entry.toast)
    }
}

// ── what the updater has said ───────────────────────────────────────────

/// The live card while Check for Updates… runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Live {
    /// Asking GitHub.
    Checking,
    /// A release found and on its way.
    Downloading {
        version: String,
        switch_back: bool,
        percent: Option<u8>,
    },
}

/// What the main window remembers about the updater between its changes.
#[derive(Clone, Debug, Default)]
pub struct UpdateWatch {
    /// Check for Updates… is waiting for the check after this many have
    /// finished (`State::finished`): the one it started, or the one that
    /// was already running.
    asked: Option<u64>,
    /// Later on this version: its card stays down. The chip still offers
    /// it.
    dismissed: Option<String>,
    /// The updater as of its last change, and the release ready then:
    /// what the window draws from.
    seen: State,
    ready: Option<Held>,
    /// `PWRDISK_UPDATE_FIXTURE` drew a set state: the updater is ignored.
    fixture: bool,
}

impl UpdateWatch {
    /// Check for Updates… was chosen while `finished` checks had ended.
    pub fn ask(&mut self, finished: u64) {
        self.asked = Some(finished);
        // Asking again is asking to see the answer again, so a release put
        // away earlier comes back rather than the check looking dead.
        self.dismissed = None;
    }

    /// Takes in the updater's state after a change, with `ready` the
    /// release Restart would install. Returns the outcome of the check
    /// Check for Updates… was waiting for, once it has finished.
    pub fn follow(
        &mut self,
        state: State,
        ready: Option<Held>,
    ) -> Option<Outcome> {
        let settled = self.asked.and_then(|after| {
            if state.checking || state.finished <= after {
                return None;
            }
            self.asked = None;
            state.last.as_ref().map(|(_, outcome)| outcome.clone())
        });
        self.seen = state;
        self.ready = ready;
        settled
    }

    /// A download's percent moved; nothing else did.
    pub fn follow_progress(&mut self, download: Option<updater::Download>) {
        self.seen.download = download;
    }

    /// The live card, while Check for Updates… waits.
    pub fn live(&self) -> Option<Live> {
        self.asked?;
        Some(match &self.seen.download {
            Some(download) => Live::Downloading {
                version: download.version.clone(),
                switch_back: download.switch_back,
                percent: download.percent,
            },
            None => Live::Checking,
        })
    }

    pub const fn is_asking(&self) -> bool {
        self.asked.is_some()
    }

    /// The release Restart to update installs, Later or not: the chip.
    pub const fn ready(&self) -> Option<&Held> {
        self.ready.as_ref()
    }

    /// The release the Ready card offers: ready and not put away.
    pub fn offer(&self) -> Option<&Held> {
        self.ready
            .as_ref()
            .filter(|ready| self.dismissed.as_deref() != Some(&ready.version))
    }

    /// Later: put away the release ready now.
    pub fn later(&mut self) {
        self.dismissed = self.ready.as_ref().map(|ready| ready.version.clone());
    }

    /// The updater as of its last change.
    pub const fn seen(&self) -> &State {
        &self.seen
    }

    pub const fn is_fixture(&self) -> bool {
        self.fixture
    }
}

// ── the updater's words ─────────────────────────────────────────────────

/// The live card's words.
pub fn live(live: &Live) -> Toast {
    match live {
        Live::Checking => Toast {
            track: Some(Track::Sweep),
            ..Toast::new(
                Tone::Accent,
                "Checking for updates",
                "Asking GitHub for the latest release\u{2026}".into(),
            )
        },
        Live::Downloading {
            version,
            switch_back,
            percent,
        } => Toast {
            track: Some(percent.map_or(Track::Sweep, Track::Percent)),
            ..Toast::new(
                Tone::Accent,
                if *switch_back {
                    "Downloading switch"
                } else {
                    "Downloading update"
                },
                match percent {
                    Some(percent) => {
                        format!("PwrDisk {version} \u{00b7} {percent}%")
                    }
                    None => format!("PwrDisk {version}"),
                },
            )
        },
    }
}

/// A download that Restart to update installs now. `marks` is how many
/// paths are marked, which a restart forgets; `restarting` once pressed,
/// `problem` why the last press did not restart.
pub fn ready(
    held: &Held,
    marks: usize,
    restarting: bool,
    problem: Option<&str>,
) -> Toast {
    let title = if held.switch_back {
        "Switch ready"
    } else {
        "Update ready"
    };
    let mut actions: Vec<Action> = release_notes(&held.version).collect();
    actions.push(Action::Restart {
        switch_back: held.switch_back,
        restarting,
    });
    actions.push(Action::Later);
    Toast {
        detail: (marks > 0).then(|| {
            let noun = if marks == 1 { "mark" } else { "marks" };
            format!("Restarting clears your {marks} {noun}.")
        }),
        problem: problem.map(|why| format!("Couldn't restart: {why}.")),
        actions,
        ..Toast::new(
            Tone::Accent,
            title,
            format!(
                "PwrDisk {} is downloaded. It installs when you restart or \
                 quit.",
                held.version
            ),
        )
    }
}

/// The answer to Check for Updates…, or `None` when the ready release is
/// the answer (its own card says it) or nothing settled.
pub fn answer(
    outcome: &Outcome,
    ready_now: Option<&Held>,
    slot: Slot,
) -> Option<Toast> {
    let timed = |tone, title: &str, body: String, actions: Vec<Action>| {
        Some(Toast {
            actions,
            timeout: Some(ANSWER),
            ..Toast::new(tone, title, body)
        })
    };
    match outcome {
        Outcome::Downloaded(_)
        | Outcome::Pending(_)
        | Outcome::SwitchReady(_)
            if ready_now.is_some() =>
        {
            None
        }
        // Held for another slot than the one in force now.
        Outcome::Downloaded(version) | Outcome::Pending(version) => timed(
            Tone::Accent,
            "Update downloaded",
            format!("{version} installs when PwrDisk quits."),
            release_notes(version).collect(),
        ),
        Outcome::SwitchReady(version) => timed(
            Tone::Accent,
            "Switch downloaded",
            format!("{version} installs from Settings \u{203a} Updates."),
            release_notes(version).collect(),
        ),
        Outcome::UpToDate(version) => timed(
            Tone::Accent,
            "PwrDisk is up to date",
            format!("{version} is the newest release on {slot}."),
            release_notes(version).collect(),
        ),
        Outcome::NothingPublished(slot) => timed(
            Tone::Accent,
            "Nothing to install",
            format!("Nothing is published on {slot} yet."),
            Vec::new(),
        ),
        Outcome::NotInstalled(_) => timed(
            Tone::Accent,
            "Updates unavailable",
            not_updatable().into(),
            vec![Action::DownloadPage],
        ),
        Outcome::Failed(why) => timed(
            Tone::Danger,
            "Update check failed",
            sentence(why),
            vec![Action::OpenLogs, Action::CheckAgain],
        ),
        // Never settles a check: the running one does.
        Outcome::AlreadyChecking => None,
    }
}

/// Why this copy cannot update itself.
const fn not_updatable() -> &'static str {
    if cfg!(debug_assertions) {
        "A development build: updates are off here."
    } else {
        "This copy wasn't installed from the DMG, Setup.exe or the MSI, so \
         it doesn't update itself."
    }
}

/// Release notes for `version`, when it is one this repository could have
/// tagged: a dev stamp gets no link onto a page that is not there.
fn release_notes(version: &str) -> impl Iterator<Item = Action> {
    crate::update_channel::parse(version)
        .map(|_| Action::ReleaseNotes(version.to_owned()))
        .into_iter()
}

/// `text` as a sentence: a capital first and a full stop last.
fn sentence(text: &str) -> String {
    let text = text.trim().trim_end_matches('.');
    let mut chars = text.chars();
    let mut out: String = chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    });
    out.push('.');
    out
}

// ── the main window's side ──────────────────────────────────────────────

/// Follows the updater (`updater::install`): the main window on every
/// change, Settings only when more than a download's percent moved, since
/// redrawing it collects the diagnostics again.
pub fn updater_changed(change: Change, cx: &mut App) {
    let windows: Vec<_> = cx
        .windows()
        .into_iter()
        .filter_map(|window| window.downcast::<Disktree>())
        .collect();
    for window in windows {
        let _ = window.update(cx, |this, _, cx| {
            this.updates_changed(change, cx);
        });
    }
    if change == Change::State {
        crate::settings::refresh(cx);
    }
}

impl Disktree {
    /// Help › Check for Updates…: the toast answers it. A check already
    /// running (the startup one, say) gives the answer.
    pub fn check_for_updates(&mut self, cx: &mut Context<'_, Self>) {
        let state = updater::state();
        self.update_watch.ask(state.finished);
        // The live card takes the last answer's place.
        self.dismiss_toast(UPDATE, cx);
        if !state.checking {
            updater::check_in_background(updater::Trigger::Manual);
        }
        cx.notify();
    }

    /// The updater's state moved. A progress tick only redraws a live
    /// card, and only while there is one; anything else is followed in
    /// full.
    pub fn updates_changed(
        &mut self,
        change: Change,
        cx: &mut Context<'_, Self>,
    ) {
        if self.update_watch.is_fixture() {
            return;
        }
        if change == Change::Progress {
            self.update_watch.follow_progress(updater::download());
            if self.update_watch.is_asking() {
                cx.notify();
            }
            return;
        }
        let installed = updater::installed_version();
        let slot = updater::selection().resolve(&installed);
        self.follow_updates(updater::state(), &installed, slot, cx);
    }

    /// [`Self::updates_changed`] for a given state, so tests can hand it
    /// one.
    pub fn follow_updates(
        &mut self,
        state: State,
        installed: &str,
        slot: Slot,
        cx: &mut Context<'_, Self>,
    ) {
        let failed_before = self.update_watch.seen().restart_failed.clone();
        let ready = updater::ready(&state, installed, slot);
        let failed_now = state.restart_failed.clone();
        if let Some(outcome) = self.update_watch.follow(state, ready.clone())
            && let Some(toast) = answer(&outcome, ready.as_ref(), slot)
        {
            self.show_toast(toast, cx);
        }
        // A failed restart also reaches the notice line, for a release
        // whose card was put away: the chip said nothing.
        if let Some(why) = failed_now.filter(|_| failed_before.is_none()) {
            self.notice = Some((
                format!("Cannot update: {why}"),
                gpui_omarchy::Status::Warning,
            ));
        }
        cx.notify();
    }

    pub fn show_toast(&mut self, toast: Toast, cx: &mut Context<'_, Self>) {
        let now = cx.background_executor().now();
        self.toasts.push(toast, now);
        self.schedule_toasts(cx);
        cx.notify();
    }

    pub fn dismiss_toast(&mut self, key: &str, cx: &mut Context<'_, Self>) {
        if self.toasts.dismiss(key) {
            self.schedule_toasts(cx);
            cx.notify();
        }
    }

    /// Later, or × on the Ready card.
    pub fn later(&mut self, cx: &mut Context<'_, Self>) {
        self.update_watch.later();
        cx.notify();
    }

    fn set_toasts_hovered(
        &mut self,
        hovered: bool,
        cx: &mut Context<'_, Self>,
    ) {
        let now = cx.background_executor().now();
        self.toasts.set_hovered(hovered, now);
        self.schedule_toasts(cx);
        cx.notify();
    }

    /// Wakes when the next toast runs out, rather than ticking.
    fn schedule_toasts(&mut self, cx: &mut Context<'_, Self>) {
        let now = cx.background_executor().now();
        if self.toasts.settle(now) {
            cx.notify();
        }
        self.toast_timer = self.toasts.next_deadline().map(|after| {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(after).await;
                let _ = this.update(cx, Self::schedule_toasts);
            })
        });
    }

    fn run_toast_action(
        &mut self,
        action: &Action,
        cx: &mut Context<'_, Self>,
    ) {
        match action {
            Action::ReleaseNotes(version) => {
                cx.open_url(&format!(
                    "{}/releases/tag/v{version}",
                    crate::help::REPOSITORY
                ));
            }
            Action::DownloadPage => {
                cx.open_url(&format!("{}/releases", crate::help::REPOSITORY));
            }
            Action::OpenLogs => cx.defer(crate::logs_window::open),
            Action::CheckAgain => self.check_for_updates(cx),
            Action::Restart { .. } => self.restart_to_update(cx),
            Action::Later => self.later(cx),
        }
    }

    /// `PWRDISK_UPDATE_FIXTURE`: draws one update state without the
    /// network, for screenshots and manual checks, and then ignores the
    /// updater. `checking`, `downloading`, `up-to-date`, `failed`, `ready`
    /// or `restarting`. An answer holds still, its strip part drained,
    /// until the pointer passes over it. Release builds keep it, as
    /// PwrRoute's `PWRROUTE_UPDATE_FIXTURE`, since screenshots are taken of
    /// one; it changes only what the window says, never what installs.
    pub fn apply_update_fixture(&mut self, cx: &mut Context<'_, Self>) {
        let Ok(kind) = std::env::var("PWRDISK_UPDATE_FIXTURE") else {
            return;
        };
        let held = Held {
            version: "0.15.0".into(),
            file_name: "PwrDisk-0.15.0-full.nupkg".into(),
            slot: Slot::default().key(),
            switch_back: false,
        };
        let mut watch = UpdateWatch {
            fixture: true,
            ..UpdateWatch::default()
        };
        let mut answer_to = None;
        match kind.as_str() {
            "checking" => {
                watch.asked = Some(0);
                watch.seen.checking = true;
            }
            "downloading" => {
                watch.asked = Some(0);
                watch.seen.checking = true;
                watch.seen.download = Some(updater::Download {
                    version: held.version,
                    switch_back: false,
                    percent: Some(42),
                });
            }
            "up-to-date" => {
                answer_to = Some(Outcome::UpToDate("0.14.0".into()));
            }
            "failed" => {
                answer_to = Some(Outcome::Failed(
                    "could not reach GitHub Releases (timed out)".into(),
                ));
            }
            "ready" => watch.ready = Some(held),
            "restarting" => {
                watch.ready = Some(held);
                watch.seen.restarting = true;
            }
            _ => return,
        }
        self.update_watch = watch;
        if let Some(toast) = answer_to
            .and_then(|outcome| answer(&outcome, None, Slot::default()))
        {
            // Pushed 3 s ago and held: the strip shows part drained.
            let now = cx.background_executor().now();
            let then = now.checked_sub(Duration::from_secs(3)).unwrap_or(now);
            self.toasts.push(toast, then);
            self.toasts.set_hovered(true, now);
        }
        cx.notify();
    }
}

/// The element id of a toast's button, for tests.
pub const fn action_id(action: &Action) -> &'static str {
    match action {
        Action::ReleaseNotes(_) => "toast-release-notes",
        Action::DownloadPage => "toast-download",
        Action::OpenLogs => "toast-open-logs",
        Action::CheckAgain => "toast-check-again",
        Action::Restart { .. } => "toast-restart",
        Action::Later => "toast-later",
    }
}

/// The update card [`UpdateWatch`] draws now, if any: the live card while
/// Check for Updates… waits, else the ready release unless an answer is in
/// its place or it was put away. A running removal keeps the ready card
/// down: Restart would wait for it anyway, and the chip still offers it.
pub fn update_card(app: &Disktree) -> Option<Toast> {
    let watch = &app.update_watch;
    if let Some(state) = watch.live() {
        return Some(live(&state));
    }
    if app.toasts.get(UPDATE).is_some() || !app.can_restart_to_update() {
        return None;
    }
    let held = watch.offer()?;
    let seen = watch.seen();
    Some(ready(
        held,
        app.marks.len(),
        seen.restarting,
        seen.restart_failed.as_deref(),
    ))
}

/// The stack, lower left of the screen's content (the caller anchors it
/// there, above the footer); `None` when there is nothing to show.
pub fn stack(
    app: &mut Disktree,
    window: &Window,
    cx: &mut Context<'_, Disktree>,
) -> Option<AnyElement> {
    let derived = update_card(app);
    if app.toasts.is_empty() && derived.is_none() {
        app.toasts.forget_hover();
        return None;
    }
    let app = &*app;
    let now = cx.background_executor().now();
    let moving = app
        .toasts
        .iter()
        .any(|toast| toast.timeout.is_some() && !app.toasts.is_hovered())
        || derived
            .as_ref()
            .is_some_and(|toast| toast.track == Some(Track::Sweep));
    // Asked only while something moves: the answer is a platform call.
    let still = moving && crate::state::prefers_reduced_motion(cx);
    let mut column = div()
        .id(STACK_ID)
        .debug_selector(|| STACK_ID.into())
        .absolute()
        .left(space::LG)
        .bottom(space::LG)
        .w(size::TOAST)
        .flex()
        .flex_col()
        .gap(space::SM)
        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
            this.set_toasts_hovered(*hovered, cx);
        }));
    let mut draining = false;
    for toast in app.toasts.iter() {
        let left = app.toasts.left(toast.key, now);
        draining |= left.is_some() && !app.toasts.is_hovered() && !still;
        column = column.child(card(app, toast, left, still, false, cx));
    }
    if let Some(toast) = &derived {
        column = column.child(card(app, toast, None, still, true, cx));
    }
    // The countdown strip drains frame by frame; it holds while the pointer
    // is over the stack, as the timeout does.
    if draining {
        window.request_animation_frame();
    }
    Some(column.into_any_element())
}

fn card(
    app: &Disktree,
    toast: &Toast,
    left: Option<f32>,
    still: bool,
    // Drawn from the updater's state rather than held in the list: ×
    // is Later.
    derived: bool,
    cx: &Context<'_, Disktree>,
) -> gpui_kit::Stateful<Div> {
    let theme = cx.omarchy();
    let edge: Hsla = match toast.tone {
        Tone::Accent => theme.accent,
        Tone::Danger => theme.danger,
    };
    let key = toast.key;
    // A live card reports work still running: it has nothing to close.
    let close = toast.track.is_none().then(|| {
        gpui_omarchy::icon_button(
            ElementId::Name(format!("toast-close-{key}").into()),
            gpui_omarchy::IconName::Close,
            "Close",
            ButtonVariant::Secondary,
            cx,
        )
        .on_click(cx.listener(move |this, _, _, cx| {
            if derived {
                this.later(cx);
            } else {
                this.dismiss_toast(key, cx);
            }
        }))
    });
    let mut actions = div().flex().flex_row().items_center().gap(space::XS);
    let mut primary_seen = false;
    for action in &toast.actions {
        // Links lead, then a spacer, then the commands, so Restart and
        // Later sit together at the right.
        let link =
            matches!(action, Action::ReleaseNotes(_) | Action::DownloadPage);
        if !link && !primary_seen {
            primary_seen = true;
            actions = actions.child(div().flex_1());
        }
        actions = actions.child(action_button(app, action, cx));
    }
    // A live status, as the window's other notices are, so a screen reader
    // speaks the answer as well as showing it. A live card's percent is
    // left out of it: "42%... 44%..." would bury the phase changes.
    let spoken = match toast.track {
        Some(Track::Percent(_)) => toast.title.clone(),
        _ => format!("{}: {}", toast.title, toast.body),
    };
    crate::semantics::status(key, spoken)
        .debug_selector(move || format!("toast-{key}"))
        .relative()
        .overflow_hidden()
        .flex()
        .flex_col()
        .gap(space::XS)
        .p(space::MD)
        .border_1()
        .border_color(edge)
        .bg(theme.surface)
        .shadow_lg()
        .when_some(left, |card, left| {
            card.child(countdown(key, if still { 1.0 } else { left }, edge))
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(space::SM)
                .child(
                    div()
                        .flex_1()
                        .font_family(theme.mono_font.clone())
                        .text_size(text::CAPTION)
                        .font_weight(FontWeight::BOLD)
                        .text_color(edge)
                        .child(toast.title.to_uppercase()),
                )
                .children(close),
        )
        .child(
            div()
                .debug_selector(move || format!("toast-{key}-body"))
                .text_color(theme.bright)
                .child(toast.body.clone()),
        )
        .when_some(toast.detail.clone(), |card, detail| {
            card.child(
                div()
                    .text_size(text::CAPTION)
                    .text_color(theme.foreground)
                    .child(detail),
            )
        })
        .when_some(toast.problem.clone(), |card, problem| {
            card.child(
                div()
                    .debug_selector(move || format!("toast-{key}-problem"))
                    .text_size(text::CAPTION)
                    .text_color(theme.danger)
                    .child(problem),
            )
        })
        .when_some(toast.track, |card, track| {
            card.child(progress(track, still, theme.accent, theme.divider()))
        })
        .when(!toast.actions.is_empty(), |card| card.child(actions))
}

/// The family's `.app-update-banner__track`: the accent over a groove. A
/// percent fills from the left; a sweep runs across, or under Reduce
/// Motion stands still at half strength.
fn progress(track: Track, still: bool, accent: Hsla, groove: Hsla) -> Div {
    let bar = || div().absolute().top_0().bottom_0().bg(accent);
    let groove = div()
        .debug_selector(|| "toast-track".into())
        .relative()
        .overflow_hidden()
        .h(size::TOAST_TRACK)
        .w_full()
        .bg(groove);
    match track {
        Track::Percent(percent) => groove.child(
            bar()
                .left_0()
                .w(relative(f32::from(percent.min(100)) / 100.0)),
        ),
        Track::Sweep if still => {
            groove.child(bar().left_0().right_0().opacity(0.5))
        }
        Track::Sweep => groove.child(bar().w(relative(SWEEP)).with_animation(
            "toast-sweep",
            Animation::new(Duration::from_millis(1100)).repeat(),
            // From just off the left edge to just off the right.
            |bar, t| bar.left(relative((1.0 + SWEEP).mul_add(t, -SWEEP))),
        )),
    }
}

/// How much of the track the sweep covers.
const SWEEP: f32 = 0.35;

/// The family's `.app-update-banner__timer`: along the top edge, as much
/// of it as there is time left.
fn countdown(key: &'static str, left: f32, color: Hsla) -> Div {
    div()
        .debug_selector(move || format!("toast-{key}-timer"))
        .absolute()
        .top_0()
        .left_0()
        .h(size::TOAST_TIMER)
        .w(relative(left))
        .bg(color)
}

fn action_button(
    app: &Disktree,
    action: &Action,
    cx: &Context<'_, Disktree>,
) -> AnyElement {
    let id = action_id(action);
    let waiting = !app.can_restart_to_update();
    let (label, variant) = match action {
        Action::ReleaseNotes(_) => ("Release notes", ButtonVariant::Secondary),
        Action::DownloadPage => ("Download page", ButtonVariant::Secondary),
        Action::OpenLogs => ("Open Logs", ButtonVariant::Outline),
        Action::CheckAgain => ("Try again", ButtonVariant::Outline),
        Action::Restart {
            restarting: true, ..
        } => ("Restarting\u{2026}", ButtonVariant::Primary),
        Action::Restart { .. } if waiting => {
            ("Restart after the removal", ButtonVariant::Primary)
        }
        Action::Restart {
            switch_back: true, ..
        } => ("Restart to switch", ButtonVariant::Primary),
        Action::Restart { .. } => ("Restart to update", ButtonVariant::Primary),
        Action::Later => ("Later", ButtonVariant::Outline),
    };
    let disabled = matches!(
        action,
        Action::Restart {
            restarting: true,
            ..
        }
    ) || (matches!(action, Action::Restart { .. }) && waiting);
    let action = action.clone();
    button(id, label, variant, cx)
        .debug_selector(move || id.into())
        .semantic_disabled(disabled)
        .on_click(cx.listener(move |this, _, _, cx| {
            if !disabled {
                this.run_toast_action(&action, cx);
            }
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use crate::updater::Download;

    fn note(key: &'static str, timeout: Option<u64>) -> Toast {
        Toast {
            key,
            timeout: timeout.map(Duration::from_secs),
            ..Toast::new(Tone::Accent, key, String::new())
        }
    }

    fn held(version: &str) -> Held {
        Held {
            version: version.to_owned(),
            file_name: format!("PwrDisk-{version}-full.nupkg"),
            slot: Slot::default().key(),
            switch_back: false,
        }
    }

    fn finished(state: &mut State, outcome: Outcome) {
        state.checking = false;
        state.download = None;
        state.finished += 1;
        state.last = Some((SystemTime::now(), outcome));
    }

    #[test]
    fn a_toast_runs_out_unless_the_pointer_rests_on_it() {
        let start = Instant::now();
        let mut toasts = Toasts::default();
        toasts.push(note("a", Some(8)), start);
        toasts.push(note("sticky", None), start);
        assert_eq!(toasts.next_deadline(), Some(Duration::from_secs(8)));

        toasts.set_hovered(true, start + Duration::from_secs(3));
        assert_eq!(toasts.next_deadline(), None, "paused while hovered");
        // Ten seconds of reading take nothing off, the strip included.
        let left = toasts.left("a", start + Duration::from_secs(13));
        assert_eq!(left, Some(5.0 / 8.0));
        toasts.set_hovered(false, start + Duration::from_secs(13));
        assert_eq!(toasts.next_deadline(), Some(Duration::from_secs(5)));
        let left = toasts.left("a", start + Duration::from_secs(17));
        assert_eq!(left, Some(1.0 / 8.0), "drains once the pointer leaves");
        assert_eq!(toasts.left("sticky", start), None, "no strip");

        assert!(toasts.settle(start + Duration::from_secs(18)));
        assert!(toasts.get("a").is_none());
        assert!(toasts.get("sticky").is_some(), "no timeout, stays");
    }

    #[test]
    fn a_key_replaces_its_toast_and_the_oldest_gives_way() {
        let now = Instant::now();
        let mut toasts = Toasts::default();
        toasts.push(note(UPDATE, None), now);
        let mut answer = note(UPDATE, Some(8));
        answer.title = "answer".into();
        toasts.push(answer, now);
        assert_eq!(toasts.iter().count(), 1);
        assert_eq!(
            toasts.get(UPDATE).map(|t| t.title.as_str()),
            Some("answer")
        );

        for key in ["b", "c", "d"] {
            toasts.push(note(key, None), now);
        }
        let keys: Vec<_> = toasts.iter().map(|toast| toast.key).collect();
        assert_eq!(keys, ["b", "c", "d"]);
        assert!(toasts.dismiss("c"));
        assert!(!toasts.dismiss("c"));
    }

    #[test]
    fn only_a_menu_check_raises_a_card_until_a_release_is_ready() {
        let mut watch = UpdateWatch::default();
        // The startup check runs and finds nothing: no card.
        let mut state = State {
            checking: true,
            ..State::default()
        };
        assert_eq!(watch.follow(state.clone(), None), None);
        assert_eq!(watch.live(), None);
        finished(&mut state, Outcome::UpToDate("0.14.0".into()));
        assert_eq!(watch.follow(state.clone(), None), None);
        assert_eq!(watch.offer(), None);
        // A background download that lands is offered.
        state.held = Some(held("0.15.0"));
        watch.follow(state, Some(held("0.15.0")));
        assert_eq!(watch.offer(), Some(&held("0.15.0")));
    }

    #[test]
    fn a_menu_check_follows_its_progress_then_settles() {
        let mut watch = UpdateWatch::default();
        let slot = Slot::default();
        let mut state = State::default();
        watch.ask(state.finished);
        state.checking = true;
        assert_eq!(watch.follow(state.clone(), None), None);
        assert_eq!(watch.live(), Some(Live::Checking));
        let download = Download {
            version: "0.15.0".into(),
            switch_back: false,
            percent: Some(42),
        };
        watch.follow_progress(Some(download));
        assert_eq!(
            watch.live(),
            Some(Live::Downloading {
                version: "0.15.0".into(),
                switch_back: false,
                percent: Some(42),
            })
        );
        let card = live(&watch.live().unwrap());
        assert_eq!(card.body, "PwrDisk 0.15.0 \u{00b7} 42%");
        assert_eq!(card.track, Some(Track::Percent(42)));
        assert_eq!(card.timeout, None, "work under way has no countdown");

        finished(&mut state, Outcome::Failed("timed out".into()));
        let outcome = watch.follow(state, None).expect("settles");
        assert_eq!(watch.live(), None);
        let failed = answer(&outcome, None, slot).expect("an answer");
        assert_eq!(failed.tone, Tone::Danger);
        assert_eq!(failed.body, "Timed out.");
        assert_eq!(failed.timeout, Some(ANSWER));
    }

    #[test]
    fn a_menu_check_waits_for_the_check_already_running() {
        let mut watch = UpdateWatch::default();
        let mut state = State {
            checking: true,
            finished: 3,
            ..State::default()
        };
        watch.ask(state.finished);
        // The running check has not ended: still waiting.
        assert_eq!(watch.follow(state.clone(), None), None);
        assert!(watch.is_asking());
        finished(&mut state, Outcome::UpToDate("0.14.0".into()));
        assert!(watch.follow(state, None).is_some());
        assert!(!watch.is_asking());
    }

    #[test]
    fn later_puts_away_one_release_and_asking_brings_it_back() {
        let mut watch = UpdateWatch::default();
        let slot = Slot::default();
        let mut state = State::default();
        watch.ask(state.finished);
        finished(&mut state, Outcome::Downloaded("0.15.0".into()));
        let ready = held("0.15.0");
        let outcome = watch
            .follow(state.clone(), Some(ready.clone()))
            .expect("settles");
        assert_eq!(
            answer(&outcome, Some(&ready), slot),
            None,
            "the ready card is the answer"
        );
        assert_eq!(watch.offer(), Some(&ready));

        watch.later();
        assert_eq!(watch.offer(), None, "put away");
        assert_eq!(watch.ready(), Some(&ready), "the chip still offers it");
        // The next background check finds the same download: still down.
        finished(&mut state, Outcome::Pending("0.15.0".into()));
        watch.follow(state.clone(), Some(ready));
        assert_eq!(watch.offer(), None);
        // A newer release comes back up.
        finished(&mut state, Outcome::Downloaded("0.15.1".into()));
        watch.follow(state.clone(), Some(held("0.15.1")));
        assert_eq!(watch.offer(), Some(&held("0.15.1")));
        // And asking again brings back one put away.
        watch.later();
        watch.ask(state.finished);
        assert_eq!(watch.offer(), Some(&held("0.15.1")));
    }

    #[test]
    fn the_ready_card_says_restarting_and_why_a_restart_failed() {
        let ready = held("0.15.0");
        let card = super::ready(&ready, 3, false, None);
        assert_eq!(card.title, "Update ready");
        assert_eq!(card.timeout, None, "stays until acted on");
        assert_eq!(
            card.detail.as_deref(),
            Some("Restarting clears your 3 marks.")
        );
        assert_eq!(card.actions.last(), Some(&Action::Later), "Later is last");
        assert_eq!(
            card.actions.first(),
            Some(&Action::ReleaseNotes("0.15.0".into()))
        );
        let pressed = super::ready(&ready, 0, true, None);
        assert!(pressed.actions.contains(&Action::Restart {
            switch_back: false,
            restarting: true
        }));
        assert_eq!(pressed.detail, None, "nothing marked, nothing to lose");
        let failed = super::ready(&ready, 0, false, Some("no updater"));
        assert_eq!(
            failed.problem.as_deref(),
            Some("Couldn't restart: no updater.")
        );
    }

    #[test]
    fn answers_say_what_a_check_found() {
        let slot = Slot::default();
        let up =
            answer(&Outcome::UpToDate("0.14.0".into()), None, slot).unwrap();
        assert_eq!(up.title, "PwrDisk is up to date");
        assert!(up.body.starts_with("0.14.0 is the newest release on "));
        assert_eq!(up.actions, [Action::ReleaseNotes("0.14.0".into())]);
        assert_eq!(up.timeout, Some(ANSWER));

        let dev =
            answer(&Outcome::NotInstalled("no manifest".into()), None, slot)
                .unwrap();
        assert_eq!(dev.title, "Updates unavailable");
        assert_eq!(dev.actions, [Action::DownloadPage]);

        let none =
            answer(&Outcome::NothingPublished("Beta".into()), None, slot)
                .unwrap();
        assert_eq!(none.title, "Nothing to install");
        assert!(answer(&Outcome::AlreadyChecking, None, slot).is_none());
        // Downloaded with no ready release (held for another slot) still
        // says something.
        let other =
            answer(&Outcome::Downloaded("0.15.0".into()), None, slot).unwrap();
        assert_eq!(other.title, "Update downloaded");
        // A dev stamp gets no release notes link.
        let stamp =
            answer(&Outcome::UpToDate("dev".into()), None, slot).unwrap();
        assert!(stamp.actions.is_empty());
    }
}

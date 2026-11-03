//! Installs and updates through Velopack, ported from `PwrRoute`.
//!
//! [`run_startup_hooks`] is Velopack's startup logic, which every Velopack
//! app runs before anything else. [`check_in_background`] asks GitHub
//! Releases for a newer version, at startup, every [`CHECK_EVERY`] and from
//! the Updates panel, and downloads it into Velopack's package cache.
//!
//! A downloaded package is never applied at launch: Velopack's startup
//! apply would install whatever it downloaded last, including a package
//! held for a channel the user has since left. The package is applied when
//! PwrDisk quits, by any route ([`install`] hooks the app's quit), or when
//! the user asks for it (the panel's Restart to update,
//! [`restart_to_update`]).
//!
//! Only a copy Velopack installed can update. `cargo run`, `make install`
//! and the Linux tarball have no Velopack manifest, so their check says so
//! and stops.
//!
//! Which release: the slot of the release channel in force
//! ([`crate::update_channel`]), the Pwr family's Stable or Beta × Latest or
//! Prerelease. A check reads the published list
//! ([`crate::github_releases`]), picks the slot's release, and points
//! Velopack at that one release's files. A release older than this copy is
//! a switch back: only Check now downloads one, and it installs only from
//! Restart to switch, never on quit. A download is recorded with its slot
//! ([`Held`], `update-download.json` beside the other settings); after
//! another slot is picked it is neither shown nor installed.
//!
//! What a check found is kept in [`state`] for the Updates panel and the
//! main window's notices (`toasts.rs`), which [`install`] tells when it
//! changes. A download's percent is a [`Change::Progress`]: only what draws
//! progress follows it, so Settings is not rebuilt a hundred times a
//! download. Apart from the channel and the held download nothing is
//! persisted, so a fresh launch starts at "not checked yet".

use std::{
    cmp::Ordering as Precedence,
    fmt, fs,
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime},
};

use gpui_kit::App;
use serde::{Deserialize, Serialize};

use velopack::{
    UpdateCheck, UpdateManager, UpdateOptions, VelopackApp, VelopackAsset,
    locator::{self, LocationContext, VelopackLocator},
    sources::{HttpSource, NoneSource},
};

use crate::github_releases;
use crate::update_channel::{self, Release, Slot, UpdateSettings};

/// The held download's record, beside the other settings.
const HELD_FILE: &str = "update-download.json";
/// The release channel the user picked, beside the other settings.
const CHANNEL_FILE: &str = "update-channel.json";

/// How often a running copy checks again after the startup check: the
/// family's "at startup and every 6 hours while running". A Velopack
/// install only; a dev build has nothing to check.
pub const CHECK_EVERY: Duration = Duration::from_hours(6);

/// One check at a time: a second "Check for Updates…" while the startup
/// check is still downloading would only contend for Velopack's lock.
static CHECKING: AtomicBool = AtomicBool::new(false);

/// A restart to update is under way: a second press waits for it.
static RESTARTING: AtomicBool = AtomicBool::new(false);

/// A check asked for while one was running, to run when it ends.
static AGAIN: Mutex<Option<Trigger>> = Mutex::new(None);

/// What [`run_startup_hooks`] tells Velopack, apart from the hooks. Kept
/// as data so a test can hold the policy without a Velopack install.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartupPolicy {
    /// Apply a downloaded package at launch, swapping it in and restarting
    /// the process. Velopack's default is `true`; PwrDisk's is `false`, so
    /// the held download's channel decides what installs (see the module
    /// docs).
    pub auto_apply: bool,
}

/// The policy PwrDisk runs with.
pub const STARTUP: StartupPolicy = StartupPolicy { auto_apply: false };

/// Velopack's startup logic. Call it first in `main`, before the window.
///
/// Setup, updates and the uninstaller start `pwrdisk.exe` with a hook
/// argument; this runs that hook and exits the process. Otherwise it
/// returns at once, also for builds Velopack did not install, and also
/// when an update was downloaded on a previous run: that waits for Quit or
/// Restart to update ([`STARTUP`]).
pub fn run_startup_hooks() {
    startup_hooks(STARTUP).run();
}

/// The builder behind [`run_startup_hooks`]: `policy`, and no hooks of
/// PwrDisk's own: it registers nothing with the system on install, so there
/// is nothing to undo on uninstall.
fn startup_hooks(policy: StartupPolicy) -> VelopackApp<'static> {
    VelopackApp::build().set_auto_apply_on_startup(policy.auto_apply)
}

/// What quitting did about a downloaded update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QuitApply {
    /// Nothing downloaded, or not a Velopack install: a plain quit.
    Nothing,
    /// Velopack's updater is waiting for this process to exit, and then
    /// swaps `version` in without starting PwrDisk again.
    Applying(String),
    /// A package was downloaded but the updater could not be started; it
    /// stays downloaded for the next quit or Restart to update.
    Failed(String),
}

/// Applies a downloaded update once this process has exited. [`install`]
/// runs it as the app quits, by any route: the menu, `q`, closing the
/// window. The updater waits for the exit on its own, so this returns at
/// once and the quit goes on; PwrDisk is not started again, and the next
/// launch is the new version.
fn apply_pending_on_quit() {
    if !first_hand_off(&HANDED_OFF) {
        return;
    }
    match apply_pending() {
        QuitApply::Nothing => {}
        QuitApply::Applying(version) => {
            log::info!("updates: installing {version} after this quit");
        }
        QuitApply::Failed(why) => {
            log::warn!(
                "updates: could not install the downloaded update: {why}"
            );
        }
    }
}

/// Whether this process has handed a downloaded update to Velopack.
static HANDED_OFF: AtomicBool = AtomicBool::new(false);

/// True the first time only. Two updaters waiting on the same exit each
/// replace the installed app, racing for Velopack's lock on the packages
/// directory; one is all a quit needs.
fn first_hand_off(handed_off: &AtomicBool) -> bool {
    !handed_off.swap(true, Ordering::SeqCst)
}

fn apply_pending() -> QuitApply {
    // The manager locates the manifest itself and refuses a copy Velopack
    // did not install; no feed is read here.
    let Ok(locator) =
        locator::auto_locate_app_manifest(LocationContext::FromCurrentExe)
    else {
        return QuitApply::Nothing;
    };
    let Ok(manager) = UpdateManager::new(NoneSource {}, None, None) else {
        return QuitApply::Nothing;
    };
    let installed = locator.get_manifest_version().to_string();
    let slot = selection().resolve(&installed);
    let held = load_held().filter(|held| package_exists(&locator, held));
    let pending = match &held {
        Some(held) => quit_package(&installed, slot, held).map(Held::asset),
        // Nothing recorded: a package an earlier build downloaded, which
        // only ever fetched its own channel's newer releases.
        None => manager.get_update_pending_restart(),
    };
    on_quit(pending, |pending| {
        manager
            // Silent: no progress window over a quit, and no restart.
            .wait_exit_then_apply_updates(pending, true, false, Vec::<String>::new())
            .map_err(|e| e.to_string())
    })
}

/// The quit path's branch: `apply` runs only for a downloaded package.
fn on_quit(
    pending: Option<VelopackAsset>,
    apply: impl FnOnce(&VelopackAsset) -> Result<(), String>,
) -> QuitApply {
    let Some(pending) = pending else {
        return QuitApply::Nothing;
    };
    match apply(&pending) {
        Ok(()) => QuitApply::Applying(pending.Version),
        Err(why) => QuitApply::Failed(why),
    }
}

/// What asked for the check, for the log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Startup,
    /// Every [`CHECK_EVERY`] while running.
    Timer,
    /// Check now, from the Updates panel or "Check for Updates…".
    Manual,
    /// Another slot was picked in the Updates panel.
    Selection,
}

impl Trigger {
    /// Check now: the one check that may switch back to an older release.
    pub const fn is_manual(self) -> bool {
        matches!(self, Self::Manual)
    }
}

/// What the updater has said so far this launch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// A check is running now.
    pub checking: bool,
    /// The last check that finished, and when.
    pub last: Option<(SystemTime, Outcome)>,
    /// The published releases the last check read, or why it could not.
    pub releases: Option<Result<Vec<Release>, String>>,
    /// The package downloaded and not installed yet, and its slot.
    pub held: Option<Held>,
    /// The download running now, if the check found one to fetch.
    pub download: Option<Download>,
    /// How many checks have finished this launch: Check for Updates…
    /// waits for the next one (`toasts.rs`).
    pub finished: u64,
    /// Restart to update was pressed and has not failed: the process is
    /// about to be replaced ([`restart_in_background`]).
    pub restarting: bool,
    /// Why the last restart to update did not happen, until the next try.
    pub restart_failed: Option<String>,
}

/// A download in progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Download {
    /// Velopack's version: "0.1.1".
    pub version: String,
    /// Older than this copy: Restart to switch installs it.
    pub switch_back: bool,
    /// 0-100, once Velopack has said.
    pub percent: Option<u8>,
}

/// What moved in [`state`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// A check started or finished, a download landed, a restart began or
    /// failed, or the channel changed: everything that reads the state
    /// redraws.
    State,
    /// Only [`State::download`]: a download began or its percent moved.
    /// Only what draws progress follows it.
    Progress,
}

static STATE: Mutex<State> = Mutex::new(State {
    checking: false,
    last: None,
    releases: None,
    held: None,
    download: None,
    finished: 0,
    restarting: false,
    restart_failed: None,
});

/// The release channel as `update-channel.json` has it; [`set_selection`]
/// keeps it current. `None` until the file is read: the defaults.
static SELECTION: Mutex<Option<UpdateSettings>> = Mutex::new(None);

/// The release channel in force: picked, or following the build.
pub fn selection() -> UpdateSettings {
    SELECTION
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .unwrap_or_default()
}

/// Follows `settings` from now on, and saves them in `dir` (normally
/// [`settings_dir`]). When the slot in force changes, the new slot is
/// checked at once, forward only, and the window redrawn: a download held
/// for the old slot is neither shown nor installed now. Says why when the
/// pick could not be saved; it holds for this session either way.
pub fn set_selection(
    settings: UpdateSettings,
    dir: Option<&Path>,
) -> Result<(), String> {
    let installed = installed_version();
    let before = selection().resolve(&installed);
    *SELECTION.lock().unwrap_or_else(PoisonError::into_inner) = Some(settings);
    if settings.resolve(&installed) != before {
        check_in_background(Trigger::Selection);
    }
    set_state(|_| {});
    let path = dir
        .ok_or("there is no settings folder to save it in")?
        .join(CHANNEL_FILE);
    write_json(&path, &settings).map_err(|error| error.to_string())
}

fn load_selection() -> Option<UpdateSettings> {
    let text = fs::read_to_string(settings_dir()?.join(CHANNEL_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// The version this copy reports: Velopack's manifest, else the crate's.
pub fn installed_version() -> String {
    match installation() {
        Installation::Velopack { version } => version,
        Installation::NotInstalled { .. } => {
            env!("CARGO_PKG_VERSION").to_owned()
        }
    }
}

/// Wakes [`install`]'s task after every change to [`state`].
static CHANGED: OnceLock<async_channel::Sender<Change>> = OnceLock::new();

/// Where the channel and the held download are kept: PwrDisk's settings
/// folder, beside the Power Efficiency preset.
pub fn settings_dir() -> Option<PathBuf> {
    crate::power::settings_path()?
        .parent()
        .map(Path::to_path_buf)
}

/// The download running now: what a progress tick changed, without
/// cloning the rest of the state.
pub fn download() -> Option<Download> {
    STATE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .download
        .clone()
}

/// The updater's state as it stands.
pub fn state() -> State {
    STATE.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

fn set_state(edit: impl FnOnce(&mut State)) {
    changed(Change::State, edit);
}

/// [`set_state`] for the download's percent: wakes only what draws it.
fn set_progress(edit: impl FnOnce(&mut Option<Download>)) {
    changed(Change::Progress, |state| edit(&mut state.download));
}

fn changed(change: Change, edit: impl FnOnce(&mut State)) {
    edit(&mut STATE.lock().unwrap_or_else(PoisonError::into_inner));
    if let Some(changed) = CHANGED.get() {
        let _ = changed.try_send(change);
    }
}

/// Reads the saved channel, runs `changed` (which redraws the window)
/// whenever the state moves, saying how, checks again every
/// [`CHECK_EVERY`] on a Velopack install, and applies a downloaded update
/// when the app quits. Call once, after the window is open.
pub fn install(cx: &App, changed: impl Fn(Change, &mut App) + 'static) {
    let (tx, rx) = async_channel::unbounded::<Change>();
    if CHANGED.set(tx).is_err() {
        return;
    }
    if let Some(settings) = load_selection() {
        *SELECTION.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(settings);
    }
    if let Some(held) = load_held() {
        set_state(|state| state.held = Some(held));
    }
    cx.on_app_quit(|_| {
        apply_pending_on_quit();
        async {}
    })
    .detach();
    cx.spawn(async move |cx| {
        while let Ok(change) = rx.recv().await {
            cx.update(|cx| changed(change, cx));
        }
    })
    .detach();
    if let Some(every) = check_every(&installation()) {
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor().timer(every).await;
                check_in_background(Trigger::Timer);
            }
        })
        .detach();
    }
}

/// How often `installation` checks again on its own: [`CHECK_EVERY`] for
/// a Velopack install, never for a copy that cannot update itself.
pub const fn check_every(installation: &Installation) -> Option<Duration> {
    match installation {
        Installation::Velopack { .. } => Some(CHECK_EVERY),
        Installation::NotInstalled { .. } => None,
    }
}

/// Where this copy came from, as far as Velopack can tell without the
/// network: its manifest version, or why there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installation {
    /// Velopack installed this copy at `version`; it can update itself.
    Velopack { version: String },
    /// No Velopack manifest: `cargo run`, `make install` or the tarball.
    NotInstalled { why: String },
}

/// Reads the manifest beside the executable. Local files only.
/// Read once: an update replaces this process rather than changing its
/// manifest under it, and the Updates panel asks on every frame it draws.
pub fn installation() -> Installation {
    static INSTALLATION: OnceLock<Installation> = OnceLock::new();
    INSTALLATION
        .get_or_init(|| {
            match locator::auto_locate_app_manifest(
                LocationContext::FromCurrentExe,
            ) {
                Ok(locator) => Installation::Velopack {
                    version: locator.get_manifest_version().to_string(),
                },
                Err(error) => Installation::NotInstalled {
                    why: error.to_string(),
                },
            }
        })
        .clone()
}

/// Check for an update, and download it, on a thread of its own: Velopack's
/// client blocks, and a download can take a while.
pub fn check_in_background(trigger: Trigger) {
    if CHECKING.swap(true, Ordering::AcqRel) {
        // A new channel picked mid-check, or Check now: the running check
        // read the slot before this, so check once more when it ends.
        if matches!(trigger, Trigger::Selection | Trigger::Manual) {
            *AGAIN.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(trigger);
        }
        log(trigger, &Outcome::AlreadyChecking);
        return;
    }
    set_state(|state| state.checking = true);
    let spawned = thread::Builder::new()
        .name("pwrdisk-updater".to_owned())
        .spawn(move || {
            let outcome = check_and_download(trigger);
            CHECKING.store(false, Ordering::Release);
            log(trigger, &outcome);
            set_state(|state| finish(state, outcome));
            let again =
                AGAIN.lock().unwrap_or_else(PoisonError::into_inner).take();
            if let Some(again) = again {
                check_in_background(again);
            }
        });
    if let Err(error) = spawned {
        CHECKING.store(false, Ordering::Release);
        let outcome = Outcome::Failed(error.to_string());
        log(trigger, &outcome);
        set_state(|state| finish(state, outcome));
    }
}

/// A check ended with `outcome`.
fn finish(state: &mut State, outcome: Outcome) {
    state.checking = false;
    state.download = None;
    state.finished += 1;
    state.last = Some((SystemTime::now(), outcome));
}

/// Installs the downloaded release and starts PwrDisk again: the Updates
/// panel's Restart to update (or Restart to switch, for an older release).
/// Velopack swaps the package and relaunches, so this returns only when
/// there is nothing to apply or it could not start.
fn restart_to_update() -> Result<(), String> {
    let locator =
        locator::auto_locate_app_manifest(LocationContext::FromCurrentExe)
            .map_err(|e| e.to_string())?;
    // The manager refuses a copy Velopack did not install; no feed is read.
    let manager = UpdateManager::new(NoneSource {}, None, None)
        .map_err(|e| e.to_string())?;
    let installed = locator.get_manifest_version().to_string();
    let slot = selection().resolve(&installed);
    let pending =
        match load_held().filter(|held| package_exists(&locator, held)) {
            Some(held) => restart_package(&installed, slot, &held)
                .map(Held::asset)
                .ok_or_else(|| {
                    "the downloaded update is not for the selected channel"
                        .to_owned()
                })?,
            None => manager
                .get_update_pending_restart()
                .ok_or_else(|| "no downloaded update to install".to_owned())?,
        };
    // The quit hook hands the same package off too. This runs off the GPUI
    // thread, so a Quit can land before Velopack exits the process: claim
    // the hand-off, or two updaters race for Velopack's lock.
    if !first_hand_off(&HANDED_OFF) {
        return Err("PwrDisk is already quitting to install it".to_owned());
    }
    manager.apply_updates_and_restart(&pending).map_err(|e| {
        // Nothing was handed off: the next quit or restart may try.
        HANDED_OFF.store(false, Ordering::SeqCst);
        e.to_string()
    })
}

/// [`restart_to_update`] on a thread of its own: Velopack exits this
/// process once the updater is waiting, so nothing waits on it, and the
/// window keeps drawing meanwhile. The toast, the strip's chip and
/// Settings all restart through this. One restart at a time
/// ([`State::restarting`]); a failed one clears it and says why, so they
/// offer Restart again.
pub fn restart_in_background() {
    if !begin(&RESTARTING) {
        return;
    }
    set_state(|state| {
        state.restarting = true;
        state.restart_failed = None;
    });
    let failed = |why: String| {
        log::warn!("updates: could not restart to update: {why}");
        RESTARTING.store(false, Ordering::Release);
        set_state(|state| restart_ended(state, why));
    };
    let spawned = thread::Builder::new()
        .name("pwrdisk-restart".to_owned())
        .spawn(move || {
            if let Err(why) = restart_to_update() {
                failed(why);
            }
        });
    if let Err(error) = spawned {
        failed(error.to_string());
    }
}

/// True for the first caller until `guard` is cleared: a double-clicked
/// Restart starts Velopack's apply once.
fn begin(guard: &AtomicBool) -> bool {
    !guard.swap(true, Ordering::AcqRel)
}

/// A restart that did not happen: offered again, with why.
fn restart_ended(state: &mut State, why: String) {
    state.restarting = false;
    state.restart_failed = Some(why);
}

/// A downloaded package and the slot it was downloaded for. Velopack keeps
/// one package at a time (a download deletes the others), so one record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Held {
    /// Velopack's version: "0.1.1".
    pub version: String,
    /// The full package's file in Velopack's packages directory.
    pub file_name: String,
    /// The slot's key ([`Slot::key`]): "stable:latest".
    pub slot: String,
    /// Older than the copy that downloaded it: installed only from Restart
    /// to switch.
    #[serde(default)]
    pub switch_back: bool,
}

impl Held {
    /// Held for `slot`.
    pub fn is_for(&self, slot: Slot) -> bool {
        Slot::from_key(&self.slot) == Some(slot)
    }

    /// The package, as Velopack's apply takes it: by its file name.
    fn asset(&self) -> VelopackAsset {
        VelopackAsset {
            Version: self.version.clone(),
            FileName: self.file_name.clone(),
            Type: "Full".to_owned(),
            ..VelopackAsset::default()
        }
    }
}

/// The download Restart to update installs now, for the Updates panel:
/// held for `slot` and not this very version.
pub fn ready(state: &State, installed: &str, slot: Slot) -> Option<Held> {
    state
        .held
        .as_ref()
        .and_then(|held| restart_package(installed, slot, held))
        .cloned()
}

/// What quitting installs: the held package when it is for `slot`, newer
/// than `installed` and not a switch back.
fn quit_package<'a>(
    installed: &str,
    slot: Slot,
    held: &'a Held,
) -> Option<&'a Held> {
    let newer =
        matches!(compare(&held.version, installed), Some(Precedence::Greater));
    (held.is_for(slot) && newer && !held.switch_back).then_some(held)
}

/// What Restart to update installs: the held package when it is for
/// `slot` and not this very version.
fn restart_package<'a>(
    installed: &str,
    slot: Slot,
    held: &'a Held,
) -> Option<&'a Held> {
    let other =
        !matches!(compare(&held.version, installed), Some(Precedence::Equal));
    (held.is_for(slot) && other).then_some(held)
}

/// `a` against `b` by `SemVer` precedence; `None` when either does not parse.
fn compare(a: &str, b: &str) -> Option<Precedence> {
    Some(update_channel::parse(a)?.cmp(&update_channel::parse(b)?))
}

/// What a check does once it knows the slot's release.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Plan {
    /// Nothing to do: `Outcome` says why.
    Done(Outcome),
    /// Download `tag`; `switch_back` for an older release.
    Download { tag: String, switch_back: bool },
}

/// The check's decision, apart from Velopack and the network: `target` is
/// the slot's release, `held` what is downloaded already.
fn plan(
    installed: &str,
    slot: Slot,
    target: Option<&Release>,
    held: Option<&Held>,
    trigger: Trigger,
) -> Plan {
    let Some(target) = target else {
        return Plan::Done(Outcome::NothingPublished(slot.to_string()));
    };
    let version = target.tag.trim_start_matches('v').to_owned();
    let held = held.filter(|held| held.is_for(slot));
    // A switch back already downloaded stays until Restart to switch; only
    // Check now looks past it.
    if let Some(held) =
        held.filter(|held| held.switch_back && !trigger.is_manual())
    {
        return Plan::Done(Outcome::SwitchReady(held.version.clone()));
    }
    match compare(&version, installed) {
        // A tag that does not parse is never taken for an older release.
        None | Some(Precedence::Equal) => {
            Plan::Done(Outcome::UpToDate(installed.to_owned()))
        }
        Some(Precedence::Less) if !trigger.is_manual() => {
            Plan::Done(Outcome::UpToDate(installed.to_owned()))
        }
        Some(Precedence::Less) => match held {
            Some(held) if held.switch_back && held.version == version => {
                Plan::Done(Outcome::SwitchReady(held.version.clone()))
            }
            _ => Plan::Download {
                tag: target.tag.clone(),
                switch_back: true,
            },
        },
        Some(Precedence::Greater) => match held {
            // As new or newer is downloaded already for this slot.
            Some(held)
                if !held.switch_back
                    && matches!(
                        compare(&held.version, &version),
                        Some(Precedence::Greater | Precedence::Equal)
                    ) =>
            {
                Plan::Done(Outcome::Pending(held.version.clone()))
            }
            _ => Plan::Download {
                tag: target.tag.clone(),
                switch_back: false,
            },
        },
    }
}

fn check_and_download(trigger: Trigger) -> Outcome {
    // The installed version (Velopack's sq.version), not the one compiled
    // in: packages are stamped separately from Cargo.toml.
    let locator = match locator::auto_locate_app_manifest(
        LocationContext::FromCurrentExe,
    ) {
        Ok(locator) => locator,
        Err(error) => return Outcome::NotInstalled(error.to_string()),
    };
    let installed = locator.get_manifest_version().to_string();
    let slot = selection().resolve(&installed);
    let releases = github_releases::fetch(&locator.get_manifest_channel());
    set_state(|state| state.releases = Some(releases.clone()));
    let releases = match releases {
        Ok(releases) => releases,
        Err(error) => return Outcome::Failed(error),
    };
    let target = update_channel::select(&releases, slot);
    let held = load_held().filter(|held| package_exists(&locator, held));
    // A package Velopack no longer has is not ready to install.
    set_state(|state| state.held.clone_from(&held));
    let (tag, switch_back) =
        match plan(&installed, slot, target.as_ref(), held.as_ref(), trigger) {
            Plan::Done(outcome) => return outcome,
            Plan::Download { tag, switch_back } => (tag, switch_back),
        };
    let options = UpdateOptions {
        AllowVersionDowngrade: switch_back,
        ..UpdateOptions::default()
    };
    let source = HttpSource::new(github_releases::download_base(&tag));
    let manager = match UpdateManager::new(source, Some(options), None) {
        Ok(manager) => manager,
        Err(error) => return Outcome::NotInstalled(error.to_string()),
    };
    match manager.check_for_updates() {
        Ok(UpdateCheck::UpdateAvailable(update)) => {
            let target = &update.TargetFullRelease;
            if let Err(error) = fetch(&manager, &update, switch_back) {
                return Outcome::Failed(error.to_string());
            }
            let held = Held {
                version: target.Version.clone(),
                file_name: target.FileName.clone(),
                slot: slot.key(),
                switch_back,
            };
            save_held(&held);
            set_state(|state| state.held = Some(held.clone()));
            if switch_back {
                Outcome::SwitchReady(held.version)
            } else {
                Outcome::Downloaded(held.version)
            }
        }
        Ok(UpdateCheck::NoUpdateAvailable | UpdateCheck::RemoteIsEmpty) => {
            Outcome::UpToDate(installed)
        }
        Err(error) => Outcome::Failed(error.to_string()),
    }
}

/// Downloads `update`, keeping [`State::download`]'s percent current. Each
/// whole percent is one [`Change::Progress`], so Velopack's finer reports
/// cost nothing on the GPUI thread.
fn fetch(
    manager: &UpdateManager,
    update: &velopack::UpdateInfo,
    switch_back: bool,
) -> Result<(), velopack::Error> {
    let version = update.TargetFullRelease.Version.clone();
    set_progress(|download| {
        *download = Some(Download {
            version,
            switch_back,
            percent: None,
        });
    });
    let (progress, percents) = std::sync::mpsc::channel::<i16>();
    // Ends when Velopack drops the sender, at the end of the download.
    let follow = thread::Builder::new()
        .name("pwrdisk-download".to_owned())
        .spawn(move || {
            let mut last = None;
            for raw in percents {
                if let Some(percent) = next_percent(last, raw) {
                    last = Some(percent);
                    set_progress(|download| {
                        if let Some(download) = download {
                            download.percent = Some(percent);
                        }
                    });
                }
            }
        });
    manager.download_updates(update, follow.is_ok().then_some(progress))
}

/// The percent to show after Velopack says `raw`, when it moved from
/// `last`; `None` when it did not.
fn next_percent(last: Option<u8>, raw: i16) -> Option<u8> {
    let percent = u8::try_from(raw.clamp(0, 100)).unwrap_or(0);
    (last != Some(percent)).then_some(percent)
}

/// Whether `held`'s package is still in Velopack's packages directory.
fn package_exists(locator: &VelopackLocator, held: &Held) -> bool {
    locator.get_packages_dir().join(&held.file_name).is_file()
}

fn held_path() -> Option<PathBuf> {
    settings_dir().map(|dir| dir.join(HELD_FILE))
}

/// The file's shape: room for more than the held download later.
#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct HeldFile {
    held: Option<Held>,
}

fn load_held() -> Option<Held> {
    read_held(&held_path()?)
}

fn read_held(path: &Path) -> Option<Held> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str::<HeldFile>(&text).ok()?.held
}

fn save_held(held: &Held) {
    let Some(path) = held_path() else {
        return;
    };
    if let Err(error) = write_held(&path, held) {
        log::warn!("updates: could not record the download: {error}");
    }
}

fn write_held(path: &Path, held: &Held) -> std::io::Result<()> {
    write_json(
        path,
        &HeldFile {
            held: Some(held.clone()),
        },
    )
}

fn write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut text =
        serde_json::to_string_pretty(value).map_err(std::io::Error::other)?;
    text.push('\n');
    fs::write(path, text)
}

/// How a check went. The version strings are Velopack's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    AlreadyChecking,
    NotInstalled(String),
    UpToDate(String),
    /// The slot (by name) has no release published yet.
    NothingPublished(String),
    Downloaded(String),
    Pending(String),
    /// An older release is downloaded; Restart to switch installs it.
    SwitchReady(String),
    Failed(String),
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyChecking => write!(f, "a check is already running"),
            Self::NotInstalled(why) => {
                write!(f, "not a Velopack install, so no updates here ({why})")
            }
            Self::UpToDate(version) => write!(f, "{version} is the latest"),
            Self::Downloaded(version) => {
                write!(
                    f,
                    "downloaded {version}; it installs when PwrDisk quits or restarts"
                )
            }
            Self::Pending(version) => {
                write!(
                    f,
                    "{version} is downloaded and installs when PwrDisk quits or restarts"
                )
            }
            Self::NothingPublished(slot) => {
                write!(f, "nothing is published on {slot} yet")
            }
            Self::SwitchReady(version) => {
                write!(
                    f,
                    "{version} is downloaded; Restart to switch installs it"
                )
            }
            Self::Failed(why) => write!(f, "check failed: {why}"),
        }
    }
}

fn log(trigger: Trigger, outcome: &Outcome) {
    let trigger = match trigger {
        Trigger::Startup => "startup",
        Trigger::Timer => "timer",
        Trigger::Manual => "check now",
        Trigger::Selection => "channel",
    };
    log::info!("updates ({trigger}): {outcome}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dev_build_has_no_installation_and_nothing_to_restart_into() {
        assert!(matches!(installation(), Installation::NotInstalled { .. }));
        assert!(restart_to_update().is_err());
    }

    #[test]
    fn startup_never_applies_a_downloaded_update() {
        // The held download's channel decides what installs: the package
        // waits for Quit or Restart to update. The builder takes the policy
        // as given; a
        // Velopack install is needed to see it act, so the policy is what
        // the test can hold.
        assert_eq!(STARTUP, StartupPolicy { auto_apply: false });
        // The builder is made, hooks and all, without a Velopack install.
        let _off = startup_hooks(STARTUP);
        let _on = startup_hooks(StartupPolicy { auto_apply: true });
    }

    #[test]
    fn quitting_with_nothing_downloaded_applies_nothing() {
        let mut applied = 0;
        let outcome = on_quit(None, |_| {
            applied += 1;
            Ok(())
        });
        assert_eq!(outcome, QuitApply::Nothing);
        assert_eq!(applied, 0);
        // A test binary is not a Velopack install: the real path says the
        // same, without starting an updater.
        assert_eq!(apply_pending(), QuitApply::Nothing);
    }

    #[test]
    fn quitting_with_a_download_hands_it_to_the_updater() {
        let asset = || VelopackAsset {
            Version: "0.2.1".to_owned(),
            ..VelopackAsset::default()
        };
        assert_eq!(
            on_quit(Some(asset()), |pending| {
                assert_eq!(pending.Version, "0.2.1");
                Ok(())
            }),
            QuitApply::Applying("0.2.1".to_owned())
        );
        assert_eq!(
            on_quit(Some(asset()), |_| Err("no updater".to_owned())),
            QuitApply::Failed("no updater".to_owned())
        );
    }

    #[test]
    fn a_quit_hands_the_update_off_once() {
        let handed_off = AtomicBool::new(false);
        assert!(first_hand_off(&handed_off));
        assert!(!first_hand_off(&handed_off), "a second updater");
    }

    #[test]
    fn a_restart_runs_once_until_it_fails() {
        let guard = AtomicBool::new(false);
        assert!(begin(&guard));
        assert!(!begin(&guard), "a second press waits");
        // A failure clears the guard and says why, so Restart is offered
        // again.
        let mut state = State {
            restarting: true,
            ..State::default()
        };
        guard.store(false, Ordering::Release);
        restart_ended(&mut state, "no updater".to_owned());
        assert!(!state.restarting);
        assert_eq!(state.restart_failed.as_deref(), Some("no updater"));
        assert!(begin(&guard), "offered again");
    }

    #[test]
    fn progress_moves_once_per_whole_percent() {
        assert_eq!(next_percent(None, 0), Some(0));
        assert_eq!(next_percent(Some(0), 0), None, "the same percent");
        assert_eq!(next_percent(Some(0), 42), Some(42));
        assert_eq!(next_percent(Some(42), 250), Some(100), "clamped");
        assert_eq!(next_percent(None, -3), Some(0), "clamped");
    }

    #[test]
    fn a_finished_check_clears_its_download_and_counts() {
        let mut state = State {
            checking: true,
            download: Some(Download {
                version: "0.15.0".into(),
                switch_back: false,
                percent: Some(99),
            }),
            ..State::default()
        };
        finish(&mut state, Outcome::Downloaded("0.15.0".into()));
        assert!(!state.checking);
        assert_eq!(state.download, None);
        assert_eq!(state.finished, 1);
    }

    #[test]
    fn only_a_velopack_install_checks_on_a_timer() {
        assert_eq!(
            check_every(&Installation::Velopack {
                version: "0.2.0".to_owned()
            }),
            Some(CHECK_EVERY)
        );
        assert_eq!(
            check_every(&Installation::NotInstalled {
                why: "no manifest".to_owned()
            }),
            None
        );
        assert_eq!(CHECK_EVERY, Duration::from_hours(6));
    }

    #[test]
    fn a_dev_build_reports_it_is_not_installed() {
        // The test binary is not in a Velopack layout, so the manager cannot
        // load and nothing reaches the network.
        assert!(matches!(
            check_and_download(Trigger::Manual),
            Outcome::NotInstalled(_)
        ));
    }

    fn held(version: &str, slot: Slot, switch_back: bool) -> Held {
        Held {
            version: version.to_owned(),
            file_name: format!("PwrDisk-{version}-osx-full.nupkg"),
            slot: slot.key(),
            switch_back,
        }
    }

    fn download(tag: &str, switch_back: bool) -> Plan {
        Plan::Download {
            tag: tag.to_owned(),
            switch_back,
        }
    }

    #[test]
    fn a_check_downloads_a_newer_release_for_the_slot() {
        let target = Release::new("v0.1.1", true);
        let slot = Slot::STABLE_PRERELEASE;
        assert_eq!(
            plan("0.1.0", slot, Some(&target), None, Trigger::Timer),
            download("v0.1.1", false)
        );
        // Already downloaded for this slot: kept, not fetched again.
        let same = held("0.1.1", slot, false);
        assert_eq!(
            plan("0.1.0", slot, Some(&target), Some(&same), Trigger::Startup),
            Plan::Done(Outcome::Pending("0.1.1".into()))
        );
        // A newer release replaces an older download.
        let older = held("0.1.0-rc.9", slot, false);
        assert_eq!(
            plan(
                "0.1.0-rc.3",
                slot,
                Some(&target),
                Some(&older),
                Trigger::Timer
            ),
            download("v0.1.1", false)
        );
        // A download for another slot does not count.
        let other = held("0.1.1", Slot::BETA_LATEST, false);
        assert_eq!(
            plan("0.1.0", slot, Some(&target), Some(&other), Trigger::Timer),
            download("v0.1.1", false)
        );
    }

    #[test]
    fn only_check_now_switches_back_to_an_older_release() {
        let target = Release::new("v0.1.0", false);
        let slot = Slot::STABLE_LATEST;
        for trigger in [Trigger::Startup, Trigger::Timer, Trigger::Selection] {
            assert_eq!(
                plan("0.2.0-alpha.1", slot, Some(&target), None, trigger),
                Plan::Done(Outcome::UpToDate("0.2.0-alpha.1".into())),
                "{trigger:?}"
            );
        }
        assert_eq!(
            plan("0.2.0-alpha.1", slot, Some(&target), None, Trigger::Manual),
            download("v0.1.0", true)
        );
        // Downloaded already: ready to switch, and a background check
        // leaves it alone.
        let back = held("0.1.0", slot, true);
        assert_eq!(
            plan(
                "0.2.0-alpha.1",
                slot,
                Some(&target),
                Some(&back),
                Trigger::Manual
            ),
            Plan::Done(Outcome::SwitchReady("0.1.0".into()))
        );
        let newer = Release::new("v0.2.0", false);
        assert_eq!(
            plan(
                "0.2.0-alpha.1",
                slot,
                Some(&newer),
                Some(&back),
                Trigger::Timer
            ),
            Plan::Done(Outcome::SwitchReady("0.1.0".into()))
        );
    }

    #[test]
    fn the_same_version_or_an_empty_slot_downloads_nothing() {
        let slot = Slot::STABLE_LATEST;
        assert_eq!(
            plan(
                "0.1.0",
                slot,
                Some(&Release::new("v0.1.0", false)),
                None,
                Trigger::Manual
            ),
            Plan::Done(Outcome::UpToDate("0.1.0".into()))
        );
        assert_eq!(
            plan("0.1.0", slot, None, None, Trigger::Manual),
            Plan::Done(Outcome::NothingPublished(
                "Stable \u{00b7} Latest".into()
            ))
        );
        // A tag that does not parse is never taken for an older release.
        assert_eq!(
            plan(
                "0.1.0",
                slot,
                Some(&Release::new("nightly", false)),
                None,
                Trigger::Manual
            ),
            Plan::Done(Outcome::UpToDate("0.1.0".into()))
        );
    }

    #[test]
    fn quit_installs_only_a_newer_download_for_the_slot_in_force() {
        let slot = Slot::STABLE_LATEST;
        let newer = held("0.1.1", slot, false);
        assert_eq!(quit_package("0.1.0", slot, &newer), Some(&newer));
        assert_eq!(quit_package("0.1.0", Slot::BETA_LATEST, &newer), None);
        assert_eq!(quit_package("0.1.1", slot, &newer), None);
        let back = held("0.0.9", slot, true);
        assert_eq!(quit_package("0.1.0", slot, &back), None);
        // Restart to switch installs it; Restart never reinstalls this copy.
        assert_eq!(restart_package("0.1.0", slot, &back), Some(&back));
        assert_eq!(restart_package("0.1.0", Slot::BETA_LATEST, &back), None);
        assert_eq!(restart_package("0.1.1", slot, &newer), None);
    }

    #[test]
    fn the_held_download_is_recorded_beside_the_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HELD_FILE);
        assert_eq!(read_held(&path), None);
        let back = held("0.0.9", Slot::BETA_PRERELEASE, true);
        write_held(&path, &back).unwrap();
        assert_eq!(read_held(&path), Some(back.clone()));
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"slot\": \"beta:prerelease\""), "{text}");
        assert!(text.contains("\"switchBack\": true"), "{text}");
        assert_eq!(back.asset().FileName, "PwrDisk-0.0.9-osx-full.nupkg");
        fs::write(&path, "{").unwrap();
        assert_eq!(read_held(&path), None, "a bad file is no download");
    }

    #[test]
    fn outcomes_read_as_log_lines() {
        assert_eq!(
            Outcome::AlreadyChecking.to_string(),
            "a check is already running"
        );
        assert_eq!(
            Outcome::Downloaded("0.0.2".to_owned()).to_string(),
            "downloaded 0.0.2; it installs when PwrDisk quits or restarts"
        );
    }
}

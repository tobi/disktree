//! What git knows about a checkout, for the selection panel.
//!
//! Before deleting an agent worktree the question is "would anything be
//! lost": uncommitted changes, stashes, commits nobody pushed. That is a
//! few git calls, run off the UI thread when [`crate::git_store`] says so,
//! each inside the probe's time limit: `git status` walks the worktree, and
//! a big enough one would otherwise keep a process busy for minutes.

use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::git_store::Probe;

/// A checkout's state, as far as losing work is concerned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitState {
    /// Paths `git status --porcelain` lists: changed, staged or untracked.
    /// `None` when asking would run a program the checkout names; see
    /// [`names_its_own_filters`].
    pub changed: Option<usize>,
    pub stashes: usize,
    /// Commits ahead of the upstream; `None` when there is no upstream.
    pub unpushed: Option<usize>,
}

impl GitState {
    /// Nothing would be lost by deleting it.
    pub const fn is_clean(&self) -> bool {
        matches!(self.changed, Some(0))
            && self.stashes == 0
            && matches!(self.unpushed, Some(0))
    }

    /// One short line: `clean, no stash`, `3 changed, 1 stash, 2 unpushed`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        parts.push(match self.changed {
            Some(0) => "clean".to_string(),
            Some(count) => format!("{count} changed"),
            None => "changes unknown".to_string(),
        });
        parts.push(match self.stashes {
            0 => "no stash".to_string(),
            1 => "1 stash".to_string(),
            count => format!("{count} stashes"),
        });
        match self.unpushed {
            Some(0) => {}
            Some(count) => parts.push(format!("{count} unpushed")),
            None => parts.push("no upstream".to_string()),
        }
        parts.join(", ")
    }
}

#[cfg(test)]
thread_local! {
    /// Calls to [`is_checkout`], for the test that holds the panel to one
    /// per selection.
    pub static CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether `path` is the top of a checkout: it has a `.git` directory, or the
/// `.git` file a worktree gets.
pub fn is_checkout(path: &Path) -> bool {
    #[cfg(test)]
    CHECKS.set(CHECKS.get() + 1);
    std::fs::symlink_metadata(path.join(".git")).is_ok()
}

/// When a probe has to stop: its time is up, or nobody wants it any more.
pub struct Limit {
    deadline: Instant,
    cancel: Arc<AtomicBool>,
}

impl Limit {
    pub fn new(within: Duration, cancel: Arc<AtomicBool>) -> Self {
        Self {
            deadline: Instant::now() + within,
            cancel,
        }
    }

    fn reached(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || Instant::now() >= self.deadline
    }

    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// A probe stopped at its [`Limit`] before git finished.
#[derive(Debug, PartialEq, Eq)]
pub struct Cut;

/// Ask git about the checkout at `path`, within `limit`.
pub fn probe(path: &Path, limit: &Limit) -> Probe {
    match state(path, limit) {
        Ok(Some(state)) => Probe::Known(state),
        Ok(None) => Probe::Unreadable,
        Err(Cut) => Probe::TimedOut,
    }
}

/// Run [`probe`] within `within` on a thread of its own. It spends most
/// of that waiting on git, which would otherwise hold one of the threads
/// GPUI's background work shares; `git_store::MAX_RUNNING` bounds how
/// many there are at once.
pub fn probe_on_thread(
    path: &Path,
    within: Duration,
    cancel: Arc<AtomicBool>,
) -> async_channel::Receiver<Probe> {
    let (sender, answer) = async_channel::bounded(1);
    let path = path.to_path_buf();
    // If no thread can be made the sender goes with the closure, and the
    // window reads the closed channel as an unreadable checkout.
    let _ = std::thread::Builder::new()
        .name("git probe".to_string())
        .spawn(move || {
            let limit = Limit::new(within, cancel);
            let _ = sender.send_blocking(probe(&path, &limit));
        });
    answer
}

/// `None` when `path` is not a checkout or git is not installed.
fn state(path: &Path, limit: &Limit) -> Result<Option<GitState>, Cut> {
    if !is_checkout(path) || !git_installed() {
        return Ok(None);
    }
    let changed = if names_its_own_filters(path, limit)? {
        None
    } else {
        // Submodules are other checkouts with configs of their own.
        let Some(status) = run(
            path,
            &["status", "--porcelain=v1", "-z", "--ignore-submodules=all"],
            limit,
        )?
        else {
            return Ok(None);
        };
        Some(status.split('\0').filter(|line| !line.is_empty()).count())
    };
    // Counted from the reflog, not listed: `stash list` is `git log`, which
    // verifies signatures with the checkout's `gpg.program` when its config
    // sets `log.showSignature`.
    let stashes = run(
        path,
        &["rev-list", "--walk-reflogs", "--count", "refs/stash"],
        limit,
    )?
    .and_then(|count| count.trim().parse().ok())
    .unwrap_or(0);
    let unpushed =
        run(path, &["rev-list", "--count", "@{upstream}..HEAD"], limit)?
            .and_then(|count| count.trim().parse().ok());
    Ok(Some(GitState {
        changed,
        stashes,
        unpushed,
    }))
}

/// Whether running `git` runs git. On macOS `/usr/bin/git` is there even
/// without the developer tools, as a stub that opens an "install the command
/// line developer tools" dialog, which selecting a checkout must not do.
/// `xcode-select -p` answers without prompting. It names the selected
/// developer directory even after that directory was deleted, so the path
/// must also still be there. Asked once.
fn git_installed() -> bool {
    static INSTALLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *INSTALLED.get_or_init(|| {
        if !cfg!(target_os = "macos") {
            return true;
        }
        Command::new("xcode-select")
            .arg("-p")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .is_ok_and(|output| {
                output.status.success()
                    && Path::new(String::from_utf8_lossy(&output.stdout).trim())
                        .is_dir()
            })
    })
}

/// Whether the checkout's own config defines filter drivers.
///
/// `git status` runs a file's `clean` filter to compare it with the index,
/// and a downloaded repository can name any program as one in `.git/config`
/// and switch it on from its `.gitattributes`. No `-c` can turn off drivers
/// whose names are not known in advance, so a checkout that defines any is
/// not asked for its status at all. Filters from the user's own config, such
/// as git-lfs, are theirs and still run. Reading config runs nothing; if it
/// cannot be read, the answer is the cautious one.
fn names_its_own_filters(path: &Path, limit: &Limit) -> Result<bool, Cut> {
    let Some(output) = git(
        path,
        &[
            "config",
            "--show-scope",
            "--includes",
            "--get-regexp",
            r"^filter\.",
        ],
        limit,
    )?
    else {
        return Ok(true);
    };
    Ok(match output.status.code() {
        // No filter anywhere.
        Some(1) => false,
        Some(0) => {
            String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                matches!(line.split('\t').next(), Some("local" | "worktree"))
            })
        }
        _ => true,
    })
}

fn run(
    path: &Path,
    args: &[&str],
    limit: &Limit,
) -> Result<Option<String>, Cut> {
    Ok(git(path, args, limit)?.and_then(|output| {
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }))
}

fn git(
    path: &Path,
    args: &[&str],
    limit: &Limit,
) -> Result<Option<Output>, Cut> {
    let mut command = Command::new("git");
    // disktree is a GUI program on Windows, with no console to lend: without
    // this, every probe flashes a console window of its own.
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(
        &mut command,
        0x0800_0000, // CREATE_NO_WINDOW
    );
    command
        .arg("-C")
        .arg(path)
        // A checkout's own config can name programs for git to run: an
        // fsmonitor on every `status`, hooks, a pager. Selecting a directory
        // in a disk viewer must not execute anything it contains, and a
        // command-line `-c` outranks the repository's config. (From
        // tobi/disktree#10.) Also a signature verifier, and whatever a
        // partial clone would run to fetch a missing object: nothing here
        // may reach the network either.
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.pager=cat",
            "-c",
            "log.showSignature=false",
            "-c",
            "gpg.program=false",
            "-c",
            "gpg.ssh.program=false",
            "-c",
            "gpg.x509.program=false",
            "-c",
            "core.sshCommand=false",
            "-c",
            "protocol.allow=never",
        ])
        .args(args)
        // Never prompt, never page, never take a lock for the index refresh.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    output_within(&mut command, limit)
}

/// How often a probe looks at whether git exited or its limit is up. Small
/// against the limit, large against what a check costs.
const POLL: Duration = Duration::from_millis(5);

/// How long output is waited for after git exits, even at the limit: the
/// reader thread needs a moment to hand over what git wrote last.
const READ_GRACE: Duration = Duration::from_millis(50);

/// Run `command` to the end, or kill it at `limit`. `Ok(None)` when it
/// could not be started.
fn output_within(
    command: &mut Command,
    limit: &Limit,
) -> Result<Option<Output>, Cut> {
    // A probe whose time is up, or that nobody wants, starts nothing more.
    if limit.reached() {
        return Err(Cut);
    }
    // Its own process group, so a cut stops what git started too: a
    // user-level filter such as `git-lfs filter-process` would otherwise
    // keep hashing after git was killed.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(command, 0);
    let Ok(mut child) = command.stdout(Stdio::piped()).spawn() else {
        return Ok(None);
    };
    // Read as it is written: a `status` listing more than a pipe holds
    // would otherwise wait on us while we wait on it. The reader is never
    // joined: anything git started that kept the pipe open would keep it
    // reading past the limit, so the output is waited for on a channel
    // instead, within what is left of the limit.
    let (sender, read) = std::sync::mpsc::channel();
    if let Some(mut stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = stdout.read_to_end(&mut out);
            let _ = sender.send(out);
        });
    }
    let ended = loop {
        match exited(&mut child) {
            Ok(true) => break true,
            Ok(false) if limit.reached() => break false,
            Ok(false) => std::thread::sleep(POLL),
            Err(_) => break false,
        }
    };
    if !ended {
        stop(&mut child);
        return if limit.reached() { Err(Cut) } else { Ok(None) };
    }
    // At least a moment, so output git finished writing just before the
    // limit is not taken for output something else holds open.
    let wait = limit.remaining().max(READ_GRACE);
    let stdout = match read.recv_timeout(wait) {
        Ok(out) => out,
        // Git exited but something it started still holds its output.
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            stop(&mut child);
            return Err(Cut);
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Vec::new(),
    };
    let Ok(status) = child.wait() else {
        return Ok(None);
    };
    Ok(Some(Output {
        status,
        stdout,
        stderr: Vec::new(),
    }))
}

/// Whether `child` has exited, without reaping it on Unix: until it is
/// reaped its id, which is also its process group's, cannot go to another
/// process, so [`stop`] cannot signal a stranger's group.
#[cfg(unix)]
#[allow(
    clippy::needless_pass_by_ref_mut,
    reason = "one signature with Windows, whose `try_wait` needs it"
)]
fn exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    let options =
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    Ok(waitid(WaitId::Pid(Pid::from_child(child)), options)?.is_some())
}

#[cfg(not(unix))]
fn exited(child: &mut std::process::Child) -> std::io::Result<bool> {
    Ok(child.try_wait()?.is_some())
}

/// Kill a probe's git and, on Unix, everything in its process group.
///
/// Windows has no process group to signal; a job object would need Win32
/// calls this crate keeps out (`unsafe` lives in `disktree-core`). What git
/// starts there talks to it over pipes, so it sees end-of-file and exits
/// when git is killed, as `git-lfs filter-process` does.
fn stop(child: &mut std::process::Child) {
    // The leader is not reaped yet (see `exited`), so the group's id is
    // still git's, and this reaches a child holding git's output too.
    #[cfg(unix)]
    let _ = rustix::process::kill_process_group(
        rustix::process::Pid::from_child(child),
        rustix::process::Signal::KILL,
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ask with time to spare, as the tests of what git says do.
    fn ask(path: &Path) -> Option<GitState> {
        let limit = Limit::new(Duration::from_mins(1), Arc::default());
        state(path, &limit).ok().flatten()
    }

    #[cfg(unix)]
    #[test]
    fn a_command_past_its_limit_is_killed_and_says_so() {
        let limit = Limit::new(Duration::from_millis(50), Arc::default());
        let started = Instant::now();
        let ran = output_within(Command::new("sleep").arg("30"), &limit);
        assert_eq!(ran.map(|output| output.is_some()), Err(Cut));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "it did not wait for the command"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_cancelled_command_stops() {
        let cancel = Arc::new(AtomicBool::new(true));
        let limit = Limit::new(Duration::from_mins(1), cancel);
        let ran = output_within(Command::new("sleep").arg("30"), &limit);
        assert_eq!(ran.map(|output| output.is_some()), Err(Cut));
    }

    #[cfg(unix)]
    #[test]
    fn output_held_open_after_exit_is_cut_at_the_limit() {
        // The shell exits at once, leaving a child that keeps stdout open.
        let limit = Limit::new(Duration::from_millis(200), Arc::default());
        let started = Instant::now();
        let ran = output_within(
            Command::new("sh").args(["-c", "sleep 30 & echo started"]),
            &limit,
        );
        assert_eq!(ran.map(|output| output.is_some()), Err(Cut));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "it did not wait for the child holding the pipe"
        );
    }

    /// Whether a process with this id is still running.
    #[cfg(unix)]
    fn alive(pid: &str) -> bool {
        Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[cfg(unix)]
    #[test]
    fn a_cut_stops_what_the_command_started() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("child");
        let script =
            format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
        let limit = Limit::new(Duration::from_millis(300), Arc::default());
        let ran =
            output_within(Command::new("sh").args(["-c", &script]), &limit);
        assert_eq!(ran.map(|output| output.is_some()), Err(Cut));
        let pid = std::fs::read_to_string(&pid_file).expect("the child ran");
        let pid = pid.trim();
        // Killed, it is reaped by whoever adopted it; give that a moment.
        let gone = (0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            !alive(pid)
        });
        assert!(gone, "the child {pid} outlived the cut");
    }

    #[cfg(unix)]
    #[test]
    fn output_larger_than_a_pipe_is_read_while_it_runs() {
        let limit = Limit::new(Duration::from_secs(30), Arc::default());
        let output = output_within(
            Command::new("sh").args(["-c", "head -c 1000000 /dev/zero"]),
            &limit,
        )
        .expect("within the limit")
        .expect("it ran");
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 1_000_000);
    }

    #[test]
    fn a_probe_on_its_own_thread_answers_on_the_channel() {
        let dir = tempfile::tempdir().expect("tempdir");
        let answer =
            probe_on_thread(dir.path(), Duration::from_mins(1), Arc::default());
        assert_eq!(answer.recv_blocking(), Ok(Probe::Unreadable));
    }

    #[test]
    fn a_probe_that_runs_out_of_time_reports_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(".git"), "gitdir: elsewhere")
            .expect("write");
        let runs = Command::new("git").arg("--version").output();
        if !git_installed() || runs.is_err() {
            return; // no git on this machine
        }
        // Already past its limit: whatever git is asked first is cut.
        let limit = Limit::new(Duration::ZERO, Arc::default());
        assert_eq!(probe(dir.path(), &limit), Probe::TimedOut);
    }

    #[test]
    fn summaries_say_what_would_be_lost() {
        let clean = GitState {
            changed: Some(0),
            unpushed: Some(0),
            ..GitState::default()
        };
        assert!(clean.is_clean());
        assert_eq!(clean.summary(), "clean, no stash");
        let busy = GitState {
            changed: Some(3),
            stashes: 1,
            unpushed: Some(2),
        };
        assert!(!busy.is_clean());
        assert_eq!(busy.summary(), "3 changed, 1 stash, 2 unpushed");
        let local = GitState {
            changed: Some(0),
            ..GitState::default()
        };
        assert_eq!(local.summary(), "clean, no stash, no upstream");
        assert!(!local.is_clean(), "unpushed history could be lost");
        let guarded = GitState {
            unpushed: Some(0),
            ..GitState::default()
        };
        assert_eq!(guarded.summary(), "changes unknown, no stash");
        assert!(!guarded.is_clean(), "unknown is not clean");
    }

    #[test]
    fn reads_a_real_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(ask(dir.path()), None, "not a checkout");
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .is_ok_and(|output| output.status.success())
        };
        if !git(&["init", "-q"]) {
            return; // no git on this machine
        }
        std::fs::write(dir.path().join("a.txt"), "hi").expect("write");
        let state = ask(dir.path()).expect("a checkout");
        assert_eq!(state.changed, Some(1), "one untracked file");
        assert_eq!(state.stashes, 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_checkout_cannot_make_git_run_its_programs() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("ran");
        let hook = dir.path().join("hook.sh");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .expect("write hook");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .is_ok_and(|output| output.status.success())
        };
        if !git_installed() || !git(&["init", "-q"]) {
            return; // no git on this machine
        }
        let hook = hook.to_string_lossy();
        assert!(git(&["config", "core.fsmonitor", &hook]));
        assert!(ask(dir.path()).is_some());
        assert!(!marker.exists(), "the checkout's fsmonitor ran");
    }

    /// A signed stash and `log.showSignature`: listing stashes the way
    /// `git stash list` does would run the checkout's `gpg.program`.
    #[cfg(unix)]
    #[test]
    fn a_checkout_cannot_make_git_verify_a_signature_with_its_program() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("ran");
        let gpg = dir.path().join("gpg.sh");
        std::fs::write(
            &gpg,
            format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
        )
        .expect("write gpg");
        std::fs::set_permissions(&gpg, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        let git = |args: &[&str], input: Option<&str>| {
            use std::io::Write as _;
            let mut child = Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .ok()?;
            let mut stdin = child.stdin.take()?;
            stdin.write_all(input.unwrap_or("").as_bytes()).ok()?;
            drop(stdin);
            let output = child.wait_with_output().ok()?;
            output.status.success().then(|| {
                String::from_utf8_lossy(&output.stdout).trim().to_owned()
            })
        };
        if !git_installed() || git(&["init", "-q"], None).is_none() {
            return; // no git on this machine
        }
        let tree = git(&["mktree"], None).expect("empty tree");
        let commit = format!(
            "tree {tree}\nauthor t <t@t> 0 +0000\ncommitter t <t@t> 0 +0000\n\
             gpgsig -----BEGIN PGP SIGNATURE-----\n \n \
             -----END PGP SIGNATURE-----\n\nstash\n"
        );
        let id = git(
            &["hash-object", "-t", "commit", "-w", "--stdin"],
            Some(&commit),
        )
        .expect("a signed commit");
        assert!(
            git(&["update-ref", "--create-reflog", "refs/stash", &id], None)
                .is_some()
        );
        let gpg = gpg.to_string_lossy();
        assert!(git(&["config", "log.showSignature", "true"], None).is_some());
        assert!(git(&["config", "gpg.program", &gpg], None).is_some());

        let state = ask(dir.path()).expect("a checkout");
        assert!(!marker.exists(), "the checkout's gpg.program ran");
        assert_eq!(state.stashes, 1, "the stash is still counted");
    }

    #[cfg(unix)]
    #[test]
    fn a_checkouts_own_filter_is_never_run() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let marker = dir.path().join("ran");
        let filter = dir.path().join("filter.sh");
        // A clean filter reads the file on stdin and writes it back.
        std::fs::write(
            &filter,
            format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
        )
        .expect("write filter");
        std::fs::set_permissions(
            &filter,
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(args)
                .output()
                .is_ok_and(|output| output.status.success())
        };
        if !git_installed() || !git(&["init", "-q"]) {
            return; // no git on this machine
        }
        std::fs::write(dir.path().join("file.txt"), "x").expect("write");
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.txt filter=evil\n",
        )
        .expect("write attributes");
        assert!(git(&["add", "."]) && git(&["commit", "-qm", "one"]));
        std::fs::remove_file(&marker).ok();
        let filter = filter.to_string_lossy();
        assert!(git(&["config", "filter.evil.clean", &filter]));
        // Same content, new timestamp: git must re-read the file, through
        // the filter, to know whether it changed.
        std::fs::write(dir.path().join("file.txt"), "x").expect("rewrite");

        let state = ask(dir.path()).expect("a checkout");
        assert_eq!(state.changed, None, "not asked");
        assert!(!marker.exists(), "the checkout's filter ran");
    }
}

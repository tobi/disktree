//! What git knows about a checkout, for the selection panel.
//!
//! Before deleting an agent worktree the question is "would anything be
//! lost": uncommitted changes, stashes, commits nobody pushed. That is three
//! cheap git calls, run off the UI thread when a checkout is selected and
//! remembered per path.

use std::path::Path;
use std::process::{Command, Stdio};

/// A checkout's state, as far as losing work is concerned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitState {
    /// Paths `git status --porcelain` lists: changed, staged or untracked.
    pub changed: usize,
    pub stashes: usize,
    /// Commits ahead of the upstream; `None` when there is no upstream.
    pub unpushed: Option<usize>,
}

impl GitState {
    /// Nothing would be lost by deleting it.
    pub const fn is_clean(&self) -> bool {
        self.changed == 0
            && self.stashes == 0
            && matches!(self.unpushed, Some(0))
    }

    /// One short line: `clean, no stash`, `3 changed, 1 stash, 2 unpushed`.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        parts.push(if self.changed == 0 {
            "clean".to_string()
        } else {
            format!("{} changed", self.changed)
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

/// Whether `path` is the top of a checkout: it has a `.git` directory, or the
/// `.git` file a worktree gets.
pub fn is_checkout(path: &Path) -> bool {
    std::fs::symlink_metadata(path.join(".git")).is_ok()
}

/// Ask git. `None` when `path` is not a checkout or git is not installed.
pub fn state(path: &Path) -> Option<GitState> {
    if !is_checkout(path) {
        return None;
    }
    let status = run(path, &["status", "--porcelain=v1", "-z"])?;
    let changed = status.split('\0').filter(|line| !line.is_empty()).count();
    let stashes =
        run(path, &["stash", "list"]).map_or(0, |list| list.lines().count());
    let unpushed = run(path, &["rev-list", "--count", "@{upstream}..HEAD"])
        .and_then(|count| count.trim().parse().ok());
    Some(GitState {
        changed,
        stashes,
        unpushed,
    })
}

fn run(path: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        // Inspecting a checkout must not run its fsmonitor hook.
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-C")
        .arg(path)
        .args(args)
        // Never prompt, never page, never take a lock for the index refresh.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_say_what_would_be_lost() {
        let clean = GitState {
            unpushed: Some(0),
            ..GitState::default()
        };
        assert!(clean.is_clean());
        assert_eq!(clean.summary(), "clean, no stash");
        let busy = GitState {
            changed: 3,
            stashes: 1,
            unpushed: Some(2),
        };
        assert!(!busy.is_clean());
        assert_eq!(busy.summary(), "3 changed, 1 stash, 2 unpushed");
        let local = GitState::default();
        assert_eq!(local.summary(), "clean, no stash, no upstream");
        assert!(!local.is_clean(), "unpushed history could be lost");
    }

    #[test]
    fn reads_a_real_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(state(dir.path()), None, "not a checkout");
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
        let state = state(dir.path()).expect("a checkout");
        assert_eq!(state.changed, 1, "one untracked file");
        assert_eq!(state.stashes, 0);
    }

    #[cfg(unix)]
    #[test]
    fn reads_state_without_running_fsmonitor() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_OPTIONAL_LOCKS", "0")
                .stdin(Stdio::null())
                .output()
                .expect("run git")
        };
        assert!(git(&["init", "-q"]).status.success());
        std::fs::write(path.join("tracked.txt"), "hi").expect("write");
        // Give status an index to refresh, so the hook would actually run.
        assert!(git(&["add", "tracked.txt"]).status.success());

        let hook = path.join(".git/fsmonitor-test");
        std::fs::write(
            &hook,
            "#!/bin/sh\nprintf invoked > .git/fsmonitor-marker\nexit 1\n",
        )
        .expect("write hook");
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700))
            .expect("make hook executable");
        assert!(
            git(&["config", "core.fsmonitor", ".git/fsmonitor-test"])
                .status
                .success()
        );

        let marker = path.join(".git/fsmonitor-marker");
        assert!(git(&["status", "--porcelain=v1", "-z"]).status.success());
        assert!(marker.exists(), "plain git status must run the test hook");
        std::fs::remove_file(&marker).expect("reset marker");

        assert_eq!(
            state(path),
            Some(GitState {
                changed: 1,
                stashes: 0,
                unpushed: None,
            })
        );
        assert!(!marker.exists(), "inspection must not run the hook");
    }
}

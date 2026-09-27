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

/// Whether `path` is the top of a checkout: it has a `.git` directory, or the
/// `.git` file a worktree gets.
pub fn is_checkout(path: &Path) -> bool {
    std::fs::symlink_metadata(path.join(".git")).is_ok()
}

/// Ask git. `None` when `path` is not a checkout or git is not installed.
pub fn state(path: &Path) -> Option<GitState> {
    if !is_checkout(path) || !git_installed() {
        return None;
    }
    let changed = if names_its_own_filters(path) {
        None
    } else {
        // Submodules are other checkouts with configs of their own.
        let status = run(
            path,
            &["status", "--porcelain=v1", "-z", "--ignore-submodules=all"],
        )?;
        Some(status.split('\0').filter(|line| !line.is_empty()).count())
    };
    // Counted from the reflog, not listed: `stash list` is `git log`, which
    // verifies signatures with the checkout's `gpg.program` when its config
    // sets `log.showSignature`.
    let stashes = run(
        path,
        &["rev-list", "--walk-reflogs", "--count", "refs/stash"],
    )
    .and_then(|count| count.trim().parse().ok())
    .unwrap_or(0);
    let unpushed = run(path, &["rev-list", "--count", "@{upstream}..HEAD"])
        .and_then(|count| count.trim().parse().ok());
    Some(GitState {
        changed,
        stashes,
        unpushed,
    })
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
fn names_its_own_filters(path: &Path) -> bool {
    let Some(output) = git(
        path,
        &[
            "config",
            "--show-scope",
            "--includes",
            "--get-regexp",
            r"^filter\.",
        ],
    ) else {
        return true;
    };
    match output.status.code() {
        // No filter anywhere.
        Some(1) => false,
        Some(0) => {
            String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                matches!(line.split('\t').next(), Some("local" | "worktree"))
            })
        }
        _ => true,
    }
}

fn run(path: &Path, args: &[&str]) -> Option<String> {
    let output = git(path, args)?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn git(path: &Path, args: &[&str]) -> Option<std::process::Output> {
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
        .stderr(Stdio::null())
        .output()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(state(dir.path()).is_some());
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

        let state = state(dir.path()).expect("a checkout");
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

        let state = state(dir.path()).expect("a checkout");
        assert_eq!(state.changed, None, "not asked");
        assert!(!marker.exists(), "the checkout's filter ran");
    }
}

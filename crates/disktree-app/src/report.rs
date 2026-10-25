//! `disktree report`: scan, print what fills the tree, and exit, without a
//! window. For a terminal, a script, or a coding agent asked to free space.

use std::io::{IsTerminal as _, Write as _};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use disktree_core::report::{Limits, Report, Scanned};
use disktree_core::scan::{ScanHandle, ScanOptions};
use disktree_core::size::{human_bytes, human_count};
use disktree_core::{access, space};

/// What `disktree report` was asked for.
#[derive(Clone, Copy, Debug, Default)]
pub struct Settings {
    pub limits: Limits,
    /// One JSON document on stdout instead of text.
    pub json: bool,
}

/// How often the progress line is redrawn: often enough to look alive,
/// rarely enough not to cost the scan anything.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

pub fn run(
    root: &Path,
    options: ScanOptions,
    settings: Settings,
) -> Result<()> {
    let metric = options.metric;
    let apparent_size = options.apparent_size;
    let scan = ScanHandle::spawn(root.to_path_buf(), options);
    // Progress goes to stderr, and only to a terminal: stdout is the report,
    // and a log or a pipe has no use for carriage returns.
    let live = std::io::stderr().is_terminal();
    let started = Instant::now();
    let mut drawn: Option<Instant> = None;
    let tree = loop {
        if let Some(outcome) = scan.poll() {
            break outcome
                .with_context(|| format!("cannot scan {}", root.display()))?;
        }
        if live && drawn.is_none_or(|at| at.elapsed() >= PROGRESS_EVERY) {
            let progress = scan.progress.snapshot();
            eprint!(
                "\r\x1b[2Kscanning {} · {} · {} files · {:.0} s",
                root.display(),
                human_bytes(progress.bytes),
                human_count(progress.files),
                started.elapsed().as_secs_f64(),
            );
            drawn = Some(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if live {
        eprint!("\r\x1b[2K");
    }

    let progress = scan.progress.snapshot();
    // Canonical, like the root it is compared with: a home reached through
    // a symlink would otherwise never shorten to `~`.
    let home = std::env::home_dir()
        .map(|home| dunce::canonicalize(&home).unwrap_or(home));
    let report = Report::new(
        &tree,
        Scanned {
            root: root.to_path_buf(),
            full_disk_access: home
                .as_deref()
                .and_then(access::full_disk_access),
            home,
            metric,
            apparent_size,
            unreadable: progress.errors,
            unreadable_paths: progress.messages,
            device: space::device_for(root),
            space: space::space_info(root).ok(),
            now: crate::state::now_seconds(),
        },
        settings.limits,
    );
    let text = if settings.json {
        let mut json = report.to_json().context("write the report")?;
        json.push('\n');
        json
    } else {
        report.to_text()
    };
    let mut stdout = std::io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        // `disktree report ~ | head` closing the pipe early is not a failure.
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        outcome => outcome.context("write the report"),
    }
}

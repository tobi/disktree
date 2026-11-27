//! Lightweight, bounded runtime error log for post-mortem study.
//!
//! Writes timestamped one-block entries to `$XDG_STATE_HOME/disktree/errors.log`
//! (or `~/.local/state/disktree/errors.log` on Linux,
//! `~/Library/Logs/disktree/errors.log` on macOS,
//! `%LOCALAPPDATA%\disktree\errors.log` on Windows). Never fails or blocks
//! the UI, and rotates once the file exceeds 2 MiB.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

#[must_use]
pub fn default_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::home_dir().map(|h| h.join("Library/Logs/disktree/errors.log"))
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|d| PathBuf::from(d).join("disktree/errors.log"))
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        if let Some(dir) =
            std::env::var_os("XDG_STATE_HOME").filter(|s| !s.is_empty())
        {
            return Some(PathBuf::from(dir).join("disktree/errors.log"));
        }
        std::env::home_dir().map(|h| h.join(".local/state/disktree/errors.log"))
    }
}

/// Append a timestamped error record to the runtime error log (no-op in tests).
pub fn record(context: &str, detail: &str) {
    if cfg!(test) {
        return;
    }
    let Some(path) = default_path() else {
        return;
    };
    let _ = record_to(&path, context, detail);
}

fn record_to(
    path: &std::path::Path,
    context: &str,
    detail: &str,
) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let rotated = path.with_extension("log.1");
        let _ = fs::rename(path, rotated);
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(
        file,
        "[{ts}] v{} | {context}
{}
---",
        env!("CARGO_PKG_VERSION"),
        detail.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_timestamped_error_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("errors.log");
        record_to(&path, "ssh:dev:/home/tobi", "allocation failed").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("ssh:dev:/home/tobi"));
        assert!(text.contains("allocation failed"));
    }
}

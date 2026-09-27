//! What the operating system keeps private from a scan.
//!
//! On macOS a process without Full Disk Access is refused parts of the home
//! directory — Mail, Messages, Safari, other apps' containers, the Trash —
//! with `EPERM`, silently. A disk tool that only counts those as errors
//! leaves the user guessing, so the app asks once whether access is granted
//! and, if not, says why folders were unreadable and where to fix it.
//!
//! On Windows the same question is whether disktree runs as an
//! administrator: only then can it read a whole drive from its file table,
//! several times faster than walking it and including folders a walk is
//! refused. Without, it walks.

use std::ffi::OsString;
use std::io;
use std::path::Path;

/// System Settings, opened at Privacy & Security › Full Disk Access.
pub const FULL_DISK_ACCESS_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles";

/// Whether this process has Full Disk Access: `Some(false)` when it is
/// refused, `None` where the question does not apply or cannot be answered.
///
/// The privacy database itself is protected by Full Disk Access, so opening
/// it for reading is the test. It never shows a prompt, unlike touching
/// Desktop or Documents would.
pub fn full_disk_access(home: &Path) -> Option<bool> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let database =
        home.join("Library/Application Support/com.apple.TCC/TCC.db");
    match std::fs::File::open(database) {
        Ok(_) => Some(true),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Some(false)
        }
        Err(_) => None,
    }
}

/// Whether this process runs as an administrator: `None` off Windows,
/// where nothing a scan does depends on it.
#[cfg(windows)]
pub fn administrator() -> Option<bool> {
    Some(crate::windows::elevated())
}

/// Whether this process runs as an administrator: `None` off Windows,
/// where nothing a scan does depends on it.
#[cfg(not(windows))]
pub const fn administrator() -> Option<bool> {
    None
}

/// Whether an administrator would read `root` from its file table: a
/// whole NTFS drive. Always `false` off Windows.
#[cfg(windows)]
pub fn file_table_readable(root: &Path) -> bool {
    crate::windows::file_table_readable(root)
}

/// Whether an administrator would read `root` from its file table: a
/// whole NTFS drive. Always `false` off Windows.
#[cfg(not(windows))]
pub const fn file_table_readable(root: &Path) -> bool {
    let _ = root;
    false
}

/// Start this program again as an administrator, with `args`, through the
/// UAC prompt. An error if the prompt is declined, or off Windows.
pub fn restart_as_administrator(args: &[OsString]) -> io::Result<()> {
    #[cfg(windows)]
    return crate::windows::run_elevated(&std::env::current_exe()?, args);
    #[cfg(not(windows))]
    {
        let _ = args;
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_macos_has_an_answer_and_a_missing_database_is_not_a_refusal() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        assert_eq!(full_disk_access(temp.path()), None);
        if !cfg!(target_os = "macos") {
            let home = std::env::home_dir().expect("a home directory");
            assert_eq!(full_disk_access(&home), None);
        }
    }
}

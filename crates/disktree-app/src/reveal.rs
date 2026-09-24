//! Show a path in the platform's file manager.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The command that shows `path`, selected where the file manager can do that.
///
/// The Finder selects the item itself with `open -R`. `xdg-open` has no
/// "select" verb, so on Linux a file opens its parent directory instead of
/// launching whatever application the file's type is bound to.
pub fn command(path: &Path) -> Command {
    if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg("-R").arg(path);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(folder_for(path));
        command
    }
}

fn folder_for(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map_or_else(|| path.to_path_buf(), Path::to_path_buf)
    }
}

/// Launch the file manager without blocking the UI. The child is reaped on
/// its own thread so no zombie outlives it.
pub fn reveal(path: &Path) -> io::Result<()> {
    let mut child = command(path).spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_shown_in_its_folder_where_selecting_is_impossible() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let file = temp.path().join("big.bin");
        std::fs::write(&file, b"x").expect("write");
        assert_eq!(folder_for(&file), temp.path());
        assert_eq!(folder_for(temp.path()), temp.path());
    }

    #[test]
    fn the_command_names_the_path() {
        let path = Path::new("/tmp/some dir");
        let command = command(path);
        let args: Vec<_> = command.get_args().collect();
        if cfg!(target_os = "macos") {
            assert_eq!(command.get_program(), "open");
            assert_eq!(args, ["-R", "/tmp/some dir"]);
        } else {
            assert_eq!(command.get_program(), "xdg-open");
        }
    }
}

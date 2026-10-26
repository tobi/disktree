//! The headless front ends must never change a file. This reads the crate's
//! own source and fails if anything that could delete, move, trash, rename
//! or write a file appears in it, or if it reaches for the window's removal
//! and export code.

use std::path::Path;

/// What the source must never contain. The test lives outside `src`, so
/// spelling them out here does not trip it.
const FORBIDDEN: [&str; 20] = [
    "removal",
    "export::",
    "trash::",
    "remove_file",
    "remove_dir",
    "rename(",
    "fs::write",
    "fs::copy",
    "File::create",
    "OpenOptions",
    "set_permissions",
    "set_len",
    "hard_link",
    "symlink(",
    "create_dir",
    "Command::new",
    "process::Command",
    "unlink",
    "rmdir",
    "truncate",
];

#[test]
fn the_crate_source_cannot_touch_files() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut checked = 0;
    for entry in std::fs::read_dir(&src).expect("src") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("source");
        for (number, line) in text.lines().enumerate() {
            // Doc comments may name what the crate refuses to do.
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            for word in FORBIDDEN {
                assert!(
                    !code.contains(word),
                    "{}:{}: `{word}` has no place in a read-only crate: {line}",
                    path.display(),
                    number + 1,
                );
            }
        }
        checked += 1;
    }
    assert!(checked >= 4, "found only {checked} source files");
}

#[test]
fn the_crate_depends_on_nothing_that_writes() {
    let manifest = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .expect("manifest");
    assert!(!manifest.contains("trash"), "no trash crate");
}

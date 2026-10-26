//! The prompt's commands, run through a real shell. Kept out of `src`,
//! which may not start processes at all (see `read_only.rs`).

#![cfg(not(windows))]

use std::path::Path;

use disktree_agent::guide::{agent_prompt, claude_code_command};

/// What a shell makes of `word`: the arguments `printf` is handed.
fn as_shell_reads(word: &str) -> String {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("printf '%s' {word}"))
        .output()
        .expect("sh runs");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A folder name a shell would run comes back from the shell as the same
/// name, and nothing it names is done.
#[test]
fn a_hostile_name_stays_a_name() {
    let temp = tempfile::tempdir().expect("tempdir");
    let sentinel = temp.path().join("SENTINEL");
    let hostile = temp.path().join(format!(
        "a $(touch {0}) `touch {0}` it's \"quoted\" $HOME\nline",
        sentinel.display()
    ));
    let exe = Path::new("/opt/disk tree/it's/disktree");

    let prompt = agent_prompt(exe, &hostile);
    // Step 1's command: the program, `scan`, then the root. The name holds
    // a newline, so the command runs over two lines of the prompt.
    let command = prompt
        .split_once("1. Run: ")
        .and_then(|(_, rest)| rest.split_once(" --json --only overview"))
        .map(|(command, _)| command)
        .expect("step 1");
    let root = command.split_once(" scan ").expect("a root argument").1;
    let program = command.split_once(" scan ").expect("a program").0;

    assert_eq!(as_shell_reads(program), exe.to_string_lossy());
    assert_eq!(as_shell_reads(root), hostile.to_string_lossy());
    assert!(!sentinel.exists(), "the shell ran part of the name");

    let add = claude_code_command(exe);
    let word = add
        .strip_prefix("claude mcp add disktree -- ")
        .and_then(|rest| rest.strip_suffix(" mcp"))
        .expect("the program");
    assert_eq!(as_shell_reads(word), exe.to_string_lossy());
}

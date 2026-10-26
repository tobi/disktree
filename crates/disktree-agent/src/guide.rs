//! Help for driving disktree from an AI agent.
//!
//! What the window hands a person who wants their agent to do this: a
//! prompt to paste into Claude, Codex, Cursor or any agent that can run
//! commands, and the settings that make disktree one of its tools.
//!
//! Every command is spelled with this machine's own path to disktree, so
//! it can be pasted as it is: someone who downloaded the app has no
//! `disktree` on their `PATH`. Paths are quoted so a shell reads them as
//! one literal word: a folder named `$(rm -rf ~)` stays a name.

use std::path::Path;

/// The operating system, as a person would name it.
const fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "Mac"
    } else if cfg!(windows) {
        "Windows PC"
    } else {
        "Linux machine"
    }
}

/// A path as one literal shell word. Single quotes, inside which POSIX
/// shells and PowerShell expand nothing: no `$`, no backticks, no `$(…)`.
/// A single quote in the path is the one character to escape, as `'\''`
/// for a POSIX shell and `''` for PowerShell, the shell agents run on
/// Windows.
fn quoted(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        format!("'{}'", text.replace('\'', "''"))
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// The executable as the first word of a command. PowerShell reads a
/// quoted first word as a string, not a program, unless `&` calls it.
fn program(exe: &Path) -> String {
    if cfg!(windows) {
        format!("& {}", quoted(exe))
    } else {
        quoted(exe)
    }
}

/// A prompt asking an agent to work out, with disktree and without
/// deleting anything, how to free space under `root`.
pub fn agent_prompt(exe: &Path, root: &Path) -> String {
    let location = quoted(exe);
    let exe = program(exe);
    let root = quoted(root);
    format!(
        "\
I want to free up disk space on my {platform}. I have disktree, a read-only \
disk usage tool, installed at:
  {location}

Please:
1. Run: {exe} scan {root} --json --only overview
   It prints a JSON report and changes nothing. A large folder can take a \
minute or two.
2. Read the report. Sizes, percentages and ages are already calculated, so \
there is no need to add anything up. \"breakdown\" splits the total by \
top-level folder, by kind of data and by age. \"reclaimable\" lists caches, \
build output, dependencies and other space that can usually be recreated, \
with what each reason means. In \"largest_dirs\", \"nested_in\" marks a \
folder already counted inside another one.
3. Look closer where it helps, for example:
   {exe} scan {root} --json --only files --min-size 1G
   {exe} scan {root} --json --only dirs --depth 2
   {exe} scan {root} --json --only dirs --reclaim any --older-than 3mo
   ({exe} scan --help lists every option.)
4. Give me a short plan, biggest wins first: what to remove, how much \
space each frees, how safe it is and why, and the exact command you would \
use. Prefer the owning tool's own cleanup (for example npm cache clean, \
Xcode's DerivedData, or a project's clean command) over deleting by hand.

Do not delete, move or change anything until I approve each item.",
        platform = platform(),
    )
}

/// A prompt for a scan made over SSH.
///
/// disktree's commands run on this computer and cannot reach the server's
/// files, so the agent works from the exported report instead, and
/// anything it proposes runs on `host`.
pub fn remote_prompt(host: &str, root: &Path) -> String {
    let host = serde_json::Value::from(host).to_string();
    let root = serde_json::Value::from(root.to_string_lossy().into_owned())
        .to_string();
    format!(
        "\
I want to free up disk space on the server {host}. I scanned {root} on it \
with disktree, a read-only disk usage tool, and exported the report as JSON \
(attached).

Please:
1. Read the report. Sizes, percentages and ages are already calculated, so \
there is no need to add anything up. \"breakdown\" splits the total by \
top-level folder, by kind of data and by age. \"reclaimable\" lists caches, \
build output, dependencies and other space that can usually be recreated, \
with what each reason means. In \"largest_dirs\", \"nested_in\" marks a \
folder already counted inside another one.
2. Give me a short plan, biggest wins first: what to remove, how much \
space each frees, how safe it is and why, and the exact command to run on \
the server. Prefer the owning tool's own cleanup over deleting by hand.
3. Every command runs on {host} over SSH, never on this computer. Before \
any destructive step, check that the hostname matches the server that was \
scanned, and stop if it does not.

Do not delete, move or change anything until I approve each item."
    )
}

/// The MCP settings for Claude Desktop, Cursor and other clients that
/// read an `mcpServers` block.
pub fn mcp_config(exe: &Path) -> String {
    let command = serde_json::Value::from(exe.to_string_lossy().into_owned());
    format!(
        "{{\n  \"mcpServers\": {{\n    \"disktree\": {{\n      \"command\": \
         {command},\n      \"args\": [\"mcp\"]\n    }}\n  }}\n}}"
    )
}

/// The one line that adds disktree to Claude Code.
pub fn claude_code_command(exe: &Path) -> String {
    format!("claude mcp add disktree -- {} mcp", quoted(exe))
}

#[cfg(test)]
mod tests {
    #[cfg(not(windows))]
    use std::path::PathBuf;

    use super::*;

    #[cfg(not(windows))]
    fn exe() -> PathBuf {
        PathBuf::from("/Applications/disktree.app/Contents/MacOS/disktree")
    }

    #[cfg(not(windows))]
    #[test]
    fn the_prompt_names_this_disktree_and_asks_before_deleting() {
        let prompt = agent_prompt(&exe(), Path::new("/Users/me"));
        assert!(prompt.contains(
            "'/Applications/disktree.app/Contents/MacOS/disktree' scan \
             '/Users/me' --json --only overview"
        ));
        assert!(prompt.contains("read-only"));
        assert!(prompt.contains("until I approve each item"));
        assert!(prompt.contains("--min-size 1G"));
    }

    #[test]
    fn a_remote_prompt_works_from_the_report_and_names_the_server() {
        let prompt = remote_prompt("dev\"box", Path::new("/srv/data"));
        assert!(prompt.contains(r#"server "dev\"box""#), "{prompt}");
        assert!(prompt.contains(r#""/srv/data""#));
        assert!(prompt.contains("exported the report as JSON"));
        assert!(prompt.contains("never on this computer"));
        assert!(!prompt.contains("scan --json"), "no local commands");
    }

    #[test]
    fn the_mcp_config_is_valid_json_with_this_path() {
        let config: serde_json::Value = serde_json::from_str(&mcp_config(
            Path::new("/opt/a \"b\"/disktree"),
        ))
        .expect("valid JSON");
        let server = &config["mcpServers"]["disktree"];
        assert_eq!(server["command"], "/opt/a \"b\"/disktree");
        assert_eq!(server["args"][0], "mcp");
    }

    #[cfg(not(windows))]
    #[test]
    fn claude_code_gets_one_command() {
        assert_eq!(
            claude_code_command(&exe()),
            "claude mcp add disktree -- \
             '/Applications/disktree.app/Contents/MacOS/disktree' mcp"
        );
    }
}

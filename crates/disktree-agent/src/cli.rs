//! `disktree scan`, `disktree summary` and `disktree mcp`: the headless
//! subcommands. The window's own binary hands them over before it opens
//! anything, so none of them needs a display.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use crate::query::{
    self, Query, Sections, check_depth, check_top, normalize_extension,
    parse_category, parse_older_than, parse_reclaim, parse_size,
};
use crate::report;

/// The subcommands this crate answers to.
pub const SUBCOMMANDS: [&str; 3] = ["scan", "summary", "mcp"];

pub const USAGE: &str = "\
disktree scan — measure a directory without opening a window (read-only)

usage: disktree scan [OPTIONS] [PATH]
       disktree summary [--json] [PATH]
       disktree mcp

arguments:
  PATH                  directory to scan (default: the home directory)

output:
      --json            print the JSON report (see docs/agent-json.md)
  -n, --top N           entries per list (default 20, at most 10000)
      --only WHAT       comma list of overview, reclaim, dirs, files or
                        all (default all)
  -d, --depth N         list only entries at most N levels below PATH;
                        deeper ones are not listed, though sizes still
                        count everything beneath

measuring:
  -a, --apparent-size   apparent length instead of allocated blocks
  -x, --one-filesystem  stay on PATH's volume (the default)
  -X, --cross-filesystems
                        also measure other volumes mounted below PATH
  -H, --no-hidden       skip dotfiles and dot-directories
  -l, --follow-links    follow symlinks
  -e, --exclude PATTERN leave out matching names, or paths if PATTERN has
                        a `/` (`*`, `**`, `?`); repeatable

filters (every one given must pass):
      --min-size SIZE   at least SIZE: 500M, 1G, 1.5GiB (units are binary)
      --ext EXT[,EXT]   files with one of these extensions; repeatable
      --older-than AGE  last written before AGE ago (30d, 6mo, 1y) or a
                        date (2025-01-31); for a directory, nothing
                        beneath it was written since
      --name PATTERN    name, or path if PATTERN has a `/`; repeatable
      --category KIND   code, agent-scratch, toolchain, synced, git, media,
                        documents, cache or other; lists only the
                        outermost matching directory
      --reclaim REASON  space disktree calls reclaimable: any, regenerable,
                        sync-history, package-store, build-output,
                        reinstallable, sandbox-layers, snapshots, trash or
                        temporary; outermost directory only
  -h, --help            show this help

Nothing is ever deleted, moved or changed: these commands only list.
";

/// Run a headless subcommand if `args` (program name skipped) starts with
/// one; `None` means the window should open as usual.
pub fn dispatch(args: &[OsString]) -> Option<i32> {
    let first = args.first()?.to_str()?;
    if !SUBCOMMANDS.contains(&first) {
        return None;
    }
    let rest = &args[1..];
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    Some(match first {
        "mcp" => {
            if rest.is_empty() {
                crate::mcp::serve_stdio()
            } else {
                let _ =
                    writeln!(stderr.lock(), "disktree mcp takes no options");
                2
            }
        }
        command => run(command, rest, &mut stdout.lock(), &mut stderr.lock()),
    })
}

/// `disktree scan` or `disktree summary`, writing to the given streams.
/// Exit status: 0 done, 1 the scan failed, 2 the command line was wrong.
pub fn run(
    command: &str,
    args: &[OsString],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let parsed = match parse(args) {
        Ok(Parsed::Help) => {
            let _ = write!(out, "{USAGE}");
            return 0;
        }
        Ok(Parsed::Run(parsed)) => parsed,
        Err(message) => {
            let _ = writeln!(err, "disktree {command}: {message}");
            let _ = writeln!(err, "try `disktree {command} --help`");
            return 2;
        }
    };
    let outcome = match command {
        "summary" => report::summary(&parsed.query.root),
        _ => report::scan_quietly(&parsed.query.root, &parsed.query.settings)
            .map_err(|error| {
                format!("cannot scan {}: {error}", parsed.query.root.display())
            })
            .and_then(|scanned| {
                report::report(
                    &scanned,
                    &parsed.query,
                    chrono::Utc::now().timestamp(),
                )
            }),
    };
    let value = match outcome {
        Ok(value) => value,
        Err(message) => {
            let _ = writeln!(err, "disktree {command}: {message}");
            return 1;
        }
    };
    let written = if parsed.json {
        write!(out, "{}", report::json_text(&value))
    } else if command == "summary" {
        writeln!(out, "{}", summary_text(&value))
    } else {
        write!(out, "{}", report::text(&value))
    };
    // A closed pipe (`| head`) is not a failure worth a message.
    i32::from(written.is_err())
}

fn summary_text(value: &serde_json::Value) -> String {
    format!(
        "{}  {} used of {} ({}%) · {} available",
        value["volume_root"]
            .as_str()
            .or_else(|| value["path"].as_str())
            .unwrap_or_default(),
        value["used"]["size"].as_str().unwrap_or_default(),
        value["capacity"]["size"].as_str().unwrap_or_default(),
        value["used_percent"],
        value["available"]["size"].as_str().unwrap_or_default(),
    )
}

/// A parsed command line.
#[derive(Debug)]
pub struct Options {
    pub query: Query,
    pub json: bool,
}

#[derive(Debug)]
enum Parsed {
    Help,
    Run(Options),
}

fn parse(args: &[OsString]) -> Result<Parsed, String> {
    let mut root: Option<PathBuf> = None;
    let mut query = Query::new(PathBuf::new());
    let mut json = false;
    // Reports write paths as JSON strings, so a path that is not Unicode
    // could not be reported faithfully anyway.
    let args: Vec<String> = args
        .iter()
        .map(|arg| {
            arg.to_str().map(str::to_owned).ok_or_else(|| {
                format!("{} is not valid Unicode", arg.display())
            })
        })
        .collect::<Result<_, _>>()?;
    let mut args = args.into_iter();
    let mut value = |flag: &str| -> Result<String, String> {
        args.next().ok_or_else(|| format!("{flag} needs a value"))
    };
    // `value` borrows `args`, so the loop takes each argument through it too.
    while let Ok(arg) = value("") {
        let flag = arg.as_str();
        match flag {
            "-h" | "--help" => return Ok(Parsed::Help),
            "--json" => json = true,
            "-a" | "--apparent-size" => query.settings.apparent_size = true,
            "-x" | "--one-filesystem" => query.settings.one_filesystem = true,
            "-X" | "--cross-filesystems" => {
                query.settings.one_filesystem = false;
            }
            "-H" | "--no-hidden" => query.settings.include_hidden = false,
            "-l" | "--follow-links" => query.settings.follow_links = true,
            "-e" | "--exclude" => query.settings.exclude.push(value(flag)?),
            "-n" | "--top" => {
                query.top = check_top(number(flag, &value(flag)?)?)?;
            }
            "-d" | "--depth" => {
                query.depth = Some(check_depth(number(flag, &value(flag)?)?)?);
            }
            "--only" => query.sections = Sections::parse(&value(flag)?)?,
            "--min-size" => {
                query.filters.min_size = Some(parse_size(&value(flag)?)?);
            }
            "--ext" => query.filters.extensions.extend(
                value(flag)?.split(',').filter_map(normalize_extension),
            ),
            "--older-than" => {
                query.filters.older_than =
                    Some(parse_older_than(&value(flag)?)?);
            }
            "--name" => query.filters.names.push(value(flag)?),
            "--category" => {
                query.filters.category = Some(parse_category(&value(flag)?)?);
            }
            "--reclaim" => {
                query.filters.reclaim = Some(parse_reclaim(&value(flag)?)?);
            }
            other if other.starts_with('-') && other.len() > 1 => {
                return Err(format!("unknown option {other}"));
            }
            path => {
                if root.is_some() {
                    return Err("only one path can be scanned".to_owned());
                }
                root = Some(query::expand_home(path));
            }
        }
    }
    let root = match root {
        Some(root) => root,
        None => {
            std::env::home_dir().ok_or("no path given and no home directory")?
        }
    };
    query.root = query::resolve_root(&root)?;
    Ok(Parsed::Run(Options { query, json }))
}

fn number(flag: &str, text: &str) -> Result<u64, String> {
    text.parse()
        .map_err(|_| format!("{flag} needs a whole number, not {text:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn options(args: &[&str]) -> Options {
        match parse(&os(args)).expect("parses") {
            Parsed::Run(options) => options,
            Parsed::Help => panic!("help"),
        }
    }

    #[test]
    fn subcommands_are_recognised_and_paths_are_left_to_the_window() {
        assert_eq!(dispatch(&os(&[])), None);
        assert_eq!(dispatch(&os(&["/tmp"])), None);
        assert_eq!(dispatch(&os(&["--depth", "2"])), None);
        assert_eq!(dispatch(&os(&["./scan"])), None);
        assert_eq!(dispatch(&os(&["mcp", "--stray"])), Some(2));
    }

    #[test]
    fn every_option_is_read() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().to_str().expect("utf-8 temp path");
        let parsed = options(&[
            path,
            "--json",
            "-a",
            "-X",
            "-H",
            "-l",
            "-e",
            "node_modules",
            "--exclude",
            "*.tmp",
            "-n",
            "5",
            "-d",
            "3",
            "--only",
            "files",
            "--min-size",
            "1G",
            "--ext",
            ".ISO,dmg",
            "--ext",
            "zip",
            "--older-than",
            "30d",
            "--name",
            "*backup*",
            "--category",
            "cache",
            "--reclaim",
            "any",
        ]);
        let query = parsed.query;
        assert!(parsed.json);
        assert_eq!(
            query.root,
            dunce::canonicalize(temp.path()).expect("canonical")
        );
        assert!(query.settings.apparent_size);
        assert!(!query.settings.one_filesystem);
        assert!(!query.settings.include_hidden);
        assert!(query.settings.follow_links);
        assert_eq!(query.settings.exclude, ["node_modules", "*.tmp"]);
        assert_eq!(query.top, 5);
        assert_eq!(query.depth, Some(3));
        assert_eq!(query.sections, Sections::FILES);
        assert_eq!(query.filters.min_size, Some(1 << 30));
        assert_eq!(query.filters.extensions, ["iso", "dmg", "zip"]);
        assert!(query.filters.older_than.is_some());
        assert_eq!(query.filters.names, ["*backup*"]);
        assert!(query.filters.category.is_some());
        assert!(query.filters.reclaim.is_some());
    }

    #[test]
    fn mistakes_are_usage_errors() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().to_str().expect("utf-8 temp path");
        for bad in [
            vec![path, "--top"],
            vec![path, "--top", "0"],
            vec![path, "--depth", "x"],
            vec![path, "--min-size", "lots"],
            vec![path, "--category", "junk"],
            vec![path, "--only", "neither"],
            vec![path, "--frobnicate"],
            vec![path, path],
            vec!["/no/such/disktree/path"],
        ] {
            let mut out = Vec::new();
            let mut err = Vec::new();
            assert_eq!(
                run("scan", &os(&bad), &mut out, &mut err),
                2,
                "{bad:?}"
            );
            assert!(out.is_empty());
            assert!(!err.is_empty());
        }
        let mut out = Vec::new();
        assert_eq!(run("scan", &os(&["--help"]), &mut out, &mut Vec::new()), 0);
        assert!(String::from_utf8_lossy(&out).contains("read-only"));
    }
}

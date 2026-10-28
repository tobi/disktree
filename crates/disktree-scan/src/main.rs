//! Headless helper: no GPUI dependency and no destructive command surface.
use std::io::{self, BufReader, BufWriter, Read as _, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["--stdio-v1"] {
        limit_resources()?;
        // A live client is not permission to monopolise the server forever,
        // including when a filesystem syscall cannot be cancelled.
        std::thread::Builder::new()
            .name("scan-deadline".into())
            .spawn(|| {
                std::thread::park_timeout(Duration::from_mins(5));
                eprintln!(
                    "Remote scan exceeded five minutes; scan a smaller path."
                );
                std::process::exit(1);
            })?;
        let mut input = BufReader::new(io::stdin());
        let session = disktree_core::remote::read_request(&mut input)?;
        if session.watch_disconnect {
            std::thread::Builder::new()
                .name("scan-connection".into())
                .spawn(move || {
                    let mut byte = [0];
                    loop {
                        match input.read(&mut byte) {
                            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                            Ok(0) => {
                                // Exit the process, not just a scan task: a
                                // blocked filesystem must not outlive SSH.
                                std::process::exit(0);
                            }
                            _ => {
                                eprintln!("Remote scan connection closed or sent unexpected input.");
                                std::process::exit(1);
                            }
                        }
                    }
                })?;
        }
        disktree_core::remote::serve_session(
            session,
            &mut BufWriter::new(io::stdout().lock()),
        )
    } else if args.first().map(String::as_str) == Some("scan") {
        scan_command(&args[1..])
    } else if args.is_empty() || args == ["--help"] || args == ["-h"] {
        println!(
            "disktree-scan — read-only scan helper\n\
                  usage:\n\
                  \x20 disktree-scan --stdio-v1          SSH helper, used by disktree ssh\n\
                  \x20 disktree-scan scan PATH [options] local scan as JSON Lines\n\
                  scan options:\n\
                  \x20 --apparent-size  report apparent length, not allocated blocks\n\
                  \x20 --no-hidden      skip dotfiles and dot-directories\n\
                  \x20 --min-size N     print only directories of at least N bytes\n\
                  Reads nothing from stdin, writes nothing but the report to \
                  stdout. No deletion commands."
        );
        Ok(())
    } else {
        Err(io::Error::other("unknown option; try --help"))
    }
}

/// `scan PATH [--apparent-size] [--no-hidden] [--min-size N]`: the same
/// scan the treemap draws, written as JSON Lines for scripts and coding
/// agents that cannot open a window. One directory per line, parent
/// before children, children largest first; the root line is the summary.
fn scan_command(args: &[String]) -> io::Result<()> {
    let mut path: Option<PathBuf> = None;
    let mut apparent_size = false;
    let mut include_hidden = true;
    let mut min_size = 0;
    let mut values = args.iter();
    while let Some(arg) = values.next() {
        match arg.as_str() {
            "--apparent-size" => apparent_size = true,
            "--no-hidden" => include_hidden = false,
            "--min-size" => {
                min_size = values
                    .next()
                    .and_then(|count| count.parse().ok())
                    .ok_or_else(|| {
                    io::Error::other("--min-size needs a byte count")
                })?;
            }
            other if other.starts_with('-') => {
                return Err(io::Error::other(format!(
                    "unknown option {other}; try --help"
                )));
            }
            other => {
                if path.replace(PathBuf::from(other)).is_some() {
                    return Err(io::Error::other("one path per scan"));
                }
            }
        }
    }
    let root = path.ok_or_else(|| io::Error::other("scan needs a path"))?;
    let mut out = BufWriter::new(io::stdout().lock());
    run_scan(&root, apparent_size, include_hidden, min_size, &mut out)?;
    out.flush()
}

/// Scan and write; split from [`scan_command`] so tests can read the bytes.
fn run_scan(
    root: &Path,
    apparent_size: bool,
    include_hidden: bool,
    min_size: u64,
    out: &mut impl Write,
) -> io::Result<()> {
    let options = disktree_core::scan::ScanOptions {
        apparent_size,
        include_hidden,
        ..disktree_core::scan::ScanOptions::default()
    };
    let tree = disktree_core::scan::scan(root, options)?;
    write_directory(root, &tree, min_size, out)
}

/// Parent first, then surviving children largest first. A directory below
/// `min_size` is left out together with everything beneath it: the totals
/// on its parent's line still count it, they are only not printed.
fn write_directory(
    dir: &Path,
    node: &disktree_core::tree::Node,
    min_size: u64,
    out: &mut impl Write,
) -> io::Result<()> {
    serde_json::to_writer(
        &mut *out,
        &serde_json::json!({
            "path": dir.display().to_string(),
            "bytes": node.bytes,
            "files": node.files,
            "dirs": node.dirs,
            "modified": node.modified,
            "read_error": node.read_error,
        }),
    )?;
    out.write_all(b"\n")?;
    for child in &node.children {
        if child.is_dir() && child.bytes >= min_size {
            write_directory(
                &dir.join(child.name.as_ref()),
                child,
                min_size,
                out,
            )?;
        }
    }
    Ok(())
}

/// Kernel-enforced, not a sampled RSS check that can miss an allocation
/// burst. Keep this in the helper: never constrain the desktop process.
#[cfg(target_os = "linux")]
fn limit_resources() -> io::Result<()> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    const MAX_ADDRESS_SPACE: u64 = 1 << 30;
    let inherited = getrlimit(Resource::As);
    let maximum = inherited.maximum.unwrap_or(u64::MAX).min(MAX_ADDRESS_SPACE);
    let current = inherited.current.unwrap_or(u64::MAX).min(maximum);
    setrlimit(
        Resource::As,
        Rlimit {
            current: Some(current),
            maximum: Some(maximum),
        },
    )?;
    // An allocation failure must not write a potentially huge core dump.
    setrlimit(
        Resource::Core,
        Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps, reason = "same fallible API as Linux")]
const fn limit_resources() -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod scan_mode_tests {
    use super::*;

    fn grow(root: &Path) {
        std::fs::create_dir_all(root.join("big/deep")).unwrap();
        std::fs::write(root.join("big/deep/blob"), vec![0; 1000]).unwrap();
        std::fs::create_dir(root.join("tiny")).unwrap();
        std::fs::write(root.join("tiny/note"), b"x").unwrap();
    }

    fn lines(out: &[u8]) -> Vec<serde_json::Value> {
        std::str::from_utf8(out)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn scan_prints_the_root_summary_first() {
        let dir = tempfile::tempdir().unwrap();
        grow(dir.path());
        let mut out: Vec<u8> = Vec::new();
        run_scan(dir.path(), false, true, 0, &mut out).unwrap();
        let report = lines(&out);
        assert_eq!(
            report[0]["path"],
            dir.path().display().to_string().as_str()
        );
        assert_eq!(report[0]["files"], 2);
        assert_eq!(report[0]["dirs"], 4);
        // Allocated blocks: a 1000-byte blob and a 1-byte note each round up
        // to a cluster, so the total is past the apparent 1001.
        assert!(report[0]["bytes"].as_u64().unwrap() >= 1001);
    }

    #[test]
    fn a_directory_below_min_size_is_pruned_with_its_children() {
        let dir = tempfile::tempdir().unwrap();
        grow(dir.path());
        let mut out: Vec<u8> = Vec::new();
        run_scan(dir.path(), false, true, 500, &mut out).unwrap();
        let report = lines(&out);
        // The root survives as the summary, big carries the blob, tiny is
        // gone even though its parent's totals still count it.
        assert_eq!(report.len(), 3, "{report:?}");
        assert!(report[1]["path"].as_str().unwrap().ends_with("big"));
        assert!(!std::str::from_utf8(&out).unwrap().contains("tiny"));
    }

    #[test]
    fn apparent_size_reports_lengths_not_blocks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("one"), b"12345").unwrap();
        let mut blocked: Vec<u8> = Vec::new();
        run_scan(dir.path(), false, true, 0, &mut blocked).unwrap();
        let mut apparent: Vec<u8> = Vec::new();
        run_scan(dir.path(), true, true, 0, &mut apparent).unwrap();
        assert_eq!(lines(&apparent)[0]["bytes"], 5);
        assert!(lines(&blocked)[0]["bytes"].as_u64().unwrap() >= 5);
    }

    #[test]
    fn a_missing_or_repeated_path_and_an_unknown_option_are_refused() {
        assert!(scan_command(&[]).is_err());
        assert!(scan_command(&["--min-size".into(), "5".into()]).is_err());
        assert!(scan_command(&["--wat".into(), "x".into()]).is_err());
        let twice = ["a".into(), "b".into()];
        assert!(scan_command(&twice).is_err());
    }
}

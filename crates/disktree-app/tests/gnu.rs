//! `disktree --du` against GNU du itself: the same fixture and arguments,
//! and the output, the diagnostics and the exit status compared byte for
//! byte. Each case runs three times: without an index, with the index the
//! first run left, and with `--max-age` answering from it.
//!
//! GNU du is found as `gdu` (Homebrew's coreutils) or as a `du` that says it
//! is GNU. Without one the comparison is skipped, loudly.
#![cfg(unix)]

use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use rustix::fs::{Mode, OFlags, mkdirat, openat};

/// The coreutils release `disktree --du` follows. Older ones differ in a few
/// places, listed in [`CHANGED_SINCE`].
const FOLLOWS: (u32, u32) = (9, 11);

/// The oldest GNU du compared against. Before 9.2, `--apparent-size` counted
/// a directory's own size, and messages began with the full `argv[0]`.
const OLDEST: (u32, u32) = (9, 4);

/// Cases whose GNU output changed after [`OLDEST`], compared only against a
/// GNU du as new as [`FOLLOWS`].
const CHANGED_SINCE: &[&[&str]] = &[
    // The maximum depth became signed: -1 no longer wraps to "everything".
    &["-d", "-1"],
    // The list of valid time styles is worded differently.
    &["--time-style=bogus", "--time"],
];

fn version(gnu: &Path) -> (u32, u32) {
    let output = Command::new(gnu).arg("--version").output().expect("run");
    let text = String::from_utf8_lossy(&output.stdout);
    let number = text
        .lines()
        .next()
        .and_then(|line| line.rsplit(' ').next())
        .unwrap_or_default();
    let mut parts = number.split('.').map(|part| part.parse().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

fn gnu_du() -> Option<PathBuf> {
    for name in ["gdu", "du"] {
        let Ok(output) = Command::new(name).arg("--version").output() else {
            continue;
        };
        if String::from_utf8_lossy(&output.stdout).contains("GNU coreutils") {
            let path = Command::new("sh")
                .args(["-c", &format!("command -v {name}")])
                .output()
                .ok()?;
            let path = String::from_utf8_lossy(&path.stdout).trim().to_owned();
            return Some(PathBuf::from(path));
        }
    }
    None
}

fn write(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, vec![b'x'; bytes]).expect("write");
}

/// A tree with each thing du treats specially.
fn fixture(root: &Path) {
    write(&root.join("a/small"), 10);
    write(&root.join("a/four-k"), 4096);
    write(&root.join("a/b/c/deep"), 70_000);
    write(&root.join("a/b/mid"), 5000);
    write(&root.join("big"), 3 * 1024 * 1024 + 7);
    write(&root.join("with space/x y"), 100);
    write(&root.join("quote's/f"), 1);
    write(&root.join("keep/logs/app.log"), 12_345);
    write(&root.join("keep/logs/old.log.gz"), 999);
    std::fs::create_dir_all(root.join("empty")).expect("mkdir");

    // A sparse file: allocation and apparent size disagree.
    let sparse = std::fs::File::create(root.join("sparse")).expect("sparse");
    sparse.set_len(10 * 1024 * 1024).expect("set_len");

    // Hard links: within one directory, across directories, and so across
    // operands.
    write(&root.join("links/original"), 50_000);
    std::fs::hard_link(root.join("links/original"), root.join("links/second"))
        .expect("link");
    std::fs::create_dir_all(root.join("elsewhere")).expect("mkdir");
    std::fs::hard_link(
        root.join("links/original"),
        root.join("elsewhere/third"),
    )
    .expect("link");

    // Symbolic links: to a file, to a directory, to nothing, and a loop.
    symlink("../big", root.join("a/to-big")).expect("symlink");
    symlink("../a/b", root.join("keep/to-b")).expect("symlink");
    symlink("nowhere", root.join("a/dangling")).expect("symlink");
    symlink("..", root.join("a/b/c/up")).expect("symlink");

    // More than 10,000 entries: fts sorts such a directory by inode.
    let many = root.join("many");
    std::fs::create_dir_all(&many).expect("mkdir");
    for n in 0..10_050 {
        std::fs::write(many.join(format!("f{n:05}")), b"").expect("write");
    }
    // A subdirectory among them, so the order of directories shows.
    write(&many.join("zz-sub/inner"), 3000);
    write(&many.join("aa-sub/inner"), 3000);

    // Deeper than PATH_MAX (1024 bytes on macOS) from the root, built a
    // level at a time since no single path can reach the bottom.
    let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY;
    std::fs::create_dir(root.join("deep")).expect("mkdir");
    let mut at =
        openat(rustix::fs::CWD, root.join("deep"), dir_flags, Mode::empty())
            .expect("open");
    for level in 0..6 {
        let name = format!("{level}{}", "d".repeat(200));
        mkdirat(&at, name.as_str(), Mode::from_raw_mode(0o755))
            .expect("mkdirat");
        at = openat(&at, name.as_str(), dir_flags, Mode::empty())
            .expect("openat");
    }
    let file = openat(
        &at,
        "bottom",
        OFlags::WRONLY | OFlags::CREATE,
        Mode::from_raw_mode(0o644),
    )
    .expect("create");
    rustix::io::write(&file, &[b'x'; 5000]).expect("write");
}

struct Harness {
    ours: PathBuf,
    gnu: PathBuf,
    cwd: PathBuf,
    index: PathBuf,
}

fn run(
    program: &Path,
    cwd: &Path,
    args: &[&str],
    env: &[(&str, &Path)],
) -> Output {
    let mut command = Command::new(program);
    if program == Path::new(env!("CARGO_BIN_EXE_disktree")) {
        command.arg("--du");
    }
    command
        .args(args)
        .current_dir(cwd)
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .env_remove("DU_BLOCK_SIZE")
        .env_remove("BLOCK_SIZE")
        .env_remove("BLOCKSIZE")
        .env_remove("POSIXLY_CORRECT")
        .env_remove("TIME_STYLE")
        .env_remove("DISKTREE_DU_MAX_AGE")
        .env_remove("DISKTREE_DU_FRESH")
        .env_remove("DISKTREE_DU_JSON");
    for (name, value) in env {
        command.env(name, value);
    }
    command.output().expect("run")
}

fn show(output: &Output) -> String {
    format!(
        "status {:?}\n--- stdout\n{}--- stderr\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Standard error with the program's name replaced by a placeholder: the
/// full path `getopt` and the "Try ... --help" line repeat, and the name
/// messages start with, which is the basename since coreutils 9.2 and the
/// full path before.
fn stderr(output: &Output, program: &Path) -> Vec<u8> {
    let base = program.file_name().expect("name").to_string_lossy();
    String::from_utf8_lossy(&output.stderr)
        .replace(&program.display().to_string(), "DU")
        .lines()
        .map(|line| match line.strip_prefix(&format!("{base}:")) {
            Some(rest) => format!("DU:{rest}\n"),
            None => format!("{line}\n"),
        })
        .collect::<String>()
        .into_bytes()
}

impl Harness {
    /// Compare one argument set; a mismatch is returned, not raised, so
    /// that one run reports every case that differs.
    fn check(&self, args: &[&str]) -> Option<String> {
        let gnu = run(&self.gnu, &self.cwd, args, &[]);
        let index = self.index.as_path();
        let one = Path::new("1");
        let hour = Path::new("1h");
        // Through the environment, as behind a `du` symlink: appended
        // flags would land after a `--`.
        let passes: [(&str, Vec<(&str, &Path)>); 3] = [
            (
                "cold",
                vec![
                    ("DISKTREE_DU_INDEX_DIR", index),
                    ("DISKTREE_DU_FRESH", one),
                ],
            ),
            ("indexed", vec![("DISKTREE_DU_INDEX_DIR", index)]),
            (
                "--max-age",
                vec![
                    ("DISKTREE_DU_INDEX_DIR", index),
                    ("DISKTREE_DU_MAX_AGE", hour),
                ],
            ),
        ];
        for (pass, env) in passes {
            let ours = run(&self.ours, &self.cwd, args, &env);
            if ours.stdout != gnu.stdout
                || stderr(&ours, &self.ours) != stderr(&gnu, &self.gnu)
                || ours.status.code() != gnu.status.code()
            {
                return Some(format!(
                    "du {} ({pass})\n=== GNU\n{}\n=== disktree\n{}",
                    args.join(" "),
                    show(&gnu),
                    show(&ours)
                ));
            }
        }
        None
    }
}

#[test]
fn matches_gnu_du() {
    let Some(gnu) = gnu_du() else {
        eprintln!("SKIPPED: no GNU du (install coreutils for gdu)");
        return;
    };
    let version = version(&gnu);
    if version < OLDEST {
        eprintln!("SKIPPED: GNU du {version:?} predates {OLDEST:?}");
        return;
    }
    let temp = tempfile::TempDir::new().expect("tempdir");
    // Canonical, so both tools print the same path when given it.
    let base = temp.path().canonicalize().expect("canonical");
    let tree = base.join("tree");
    fixture(&tree);
    let locked = tree.join("locked");
    write(&locked.join("secret"), 100);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
        .expect("chmod");
    // Readable but not searchable: its names can be listed, not stat'ed.
    let listed = tree.join("listed");
    write(&listed.join("one"), 100);
    write(&listed.join("two"), 100);
    std::fs::set_permissions(&listed, std::fs::Permissions::from_mode(0o644))
        .expect("chmod");

    // A link named `du` selects the multicall personality. The comparison
    // normalizes program names because Homebrew installs GNU du as `gdu`.
    let bin = base.join("bin");
    std::fs::create_dir_all(&bin).expect("mkdir");
    let ours = bin.join("du");
    symlink(env!("CARGO_BIN_EXE_disktree"), &ours).expect("symlink");
    let harness = Harness {
        ours,
        gnu,
        cwd: tree,
        index: base.join("index"),
    };

    let exclude_file = base.join("excludes");
    std::fs::write(&exclude_file, "*.gz  \r\n\n\nmid\n").expect("write");
    let exclude_from = format!("--exclude-from={}", exclude_file.display());
    let names0 = base.join("names0");
    std::fs::write(&names0, b"a\0big\0\0links\0").expect("write");
    let files0 = format!("--files0-from={}", names0.display());

    let cases: &[&[&str]] = &[
        &[],
        &["."],
        &["-a"],
        &["-a", "."],
        &["-s"],
        &["-sh", "a", "big", "sparse", "links", "many"],
        &["-ah"],
        &["--si", "-a"],
        &["-ab"],
        &["-a", "--apparent-size"],
        &["-ak"],
        &["-am"],
        &["-a", "-B1K"],
        &["-a", "-BK"],
        &["-a", "-BKB"],
        &["-a", "-B", "MiB"],
        &["-a", "--block-size=human-readable"],
        &["-c", "a", "links", "elsewhere"],
        &["-sc", "links", "elsewhere", "links"],
        &["-al", "links", "elsewhere"],
        &["-d", "1"],
        &["--max-depth=0", "-a"],
        &["-d", "2", "-a", "a"],
        &["-S"],
        &["-Sa"],
        &["-S", "--apparent-size", "-a"],
        &["--inodes"],
        &["--inodes", "-a", "-h"],
        &["-a", "--exclude=*.log"],
        &["-a", "--exclude=b"],
        &["-a", "--exclude=a/b/c"],
        &["-a", &exclude_from],
        &["-a", "-t", "4K"],
        &["-a", "-t", "-4K"],
        &["-a", "-0"],
        &["-aL", "a"],
        &["-aL", "keep"],
        &["-aH", "keep/to-b"],
        &["-a", "keep/to-b"],
        &["-aD", "a/to-big"],
        &["-s", "a/dangling"],
        &["-sL", "a/dangling"],
        &["-aLl", "a"],
        &["-ax"],
        &["-a", "--time"],
        &["-a", "--time=ctime", "--time-style=full-iso"],
        &["-s", "--time", "--time-style=+%Y %j %N"],
        &["-a", "--time-style=iso", "--time"],
        &["-s", "missing", "a", "", "big"],
        &["-s", "a//"],
        &["-s", &files0],
        &["-s", "locked"],
        &["-a", "locked"],
        &["-as"],
        &["-s", "-d", "1"],
        &["-s", "-d", "0"],
        &["-d", "x"],
        &["-d", "-1"],
        &["-B", "0"],
        &["-t", "1x"],
        &["-t", "-0"],
        &["--s"],
        &["--foo", "-z"],
        &["--all=1"],
        &["--time=x"],
        &["--time-style=bogus", "--time"],
        &["--files0-from=/nonexistent"],
        &["-X", "/nonexistent"],
        &["-s", "--files0-from=/dev/null", "a"],
        &["--max-d=1", "--sum"],
        &["--", "-a"],
        &["a", "-s"],
        &["--files0-from=a", "-c"],
        &["--s=1"],
        &["-a", "listed"],
        &["-s", "deep"],
        &["-a", "--exclude=./a/b", "./a"],
        &["-a", "--exclude=./a/b", "a"],
    ];
    let failures: Vec<String> = cases
        .iter()
        .filter(|case| version >= FOLLOWS || !CHANGED_SINCE.contains(case))
        .filter_map(|case| harness.check(case))
        .collect();

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    std::fs::set_permissions(&listed, std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    assert!(
        failures.is_empty(),
        "{} of {} cases differ:\n\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

#[test]
fn json_describes_what_du_prints() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let root = temp.path();
    write(&root.join("proj/Cargo.toml"), 10);
    write(&root.join("proj/target/debug/app"), 50_000);
    write(&root.join("proj/src/main.rs"), 100);
    let output = run(
        Path::new(env!("CARGO_BIN_EXE_disktree")),
        root,
        &["--json", "-d", "1", "proj"],
        &[("DISKTREE_DU_INDEX_DIR", &root.join("index"))],
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{}", show(&output));
    assert!(text.contains("\"source\": \"walk\""), "{text}");
    assert!(
        text.contains("\"reclaim\": \"build output\"")
            && text.contains("cargo clean --manifest-path proj/Cargo.toml"),
        "{text}"
    );
}

/// Set a file's modification time `seconds` into the past.
fn age(path: &Path, seconds: u64) {
    let when =
        std::time::SystemTime::now() - std::time::Duration::from_secs(seconds);
    std::fs::File::options()
        .append(true)
        .open(path)
        .and_then(|file| file.set_modified(when))
        .expect("set mtime");
}

fn source_of(output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stdout);
    let at = text.find("\"source\": \"").expect("a source") + 11;
    text[at..].split('"').next().unwrap_or_default().to_owned()
}

/// `--max-age` catches up on what changed since the snapshot and answers
/// what a walk would, without walking.
fn catches_up(journal: bool) {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let root = temp.path().canonicalize().expect("canonical");
    for dir in ["a/b/c", "a/d", "e"] {
        write(&root.join(dir).join("old"), 20_000);
        age(&root.join(dir).join("old"), 7200);
    }
    write(&root.join("e/gone"), 9000);
    let index = root.join("index");
    let du = Path::new(env!("CARGO_BIN_EXE_disktree"));
    let off = Path::new("0");
    let mut env = vec![("DISKTREE_DU_INDEX_DIR", index.as_path())];
    if !journal {
        env.push(("DISKTREE_DU_JOURNAL", off));
    }
    let args = ["-a", "a", "e"];
    run(du, &root, &args, &env);

    // A new file deep down, one removed, and an old file grown in place,
    // which leaves every directory's timestamps alone.
    write(&root.join("a/b/c/new"), 50_000);
    std::fs::remove_file(root.join("e/gone")).expect("remove");
    if journal {
        std::fs::OpenOptions::new()
            .append(true)
            .open(root.join("a/d/old"))
            .and_then(|mut f| {
                std::io::Write::write_all(&mut f, &vec![1; 300_000])
            })
            .expect("grow");
    }
    std::thread::sleep(std::time::Duration::from_millis(500));

    let hour = Path::new("1h");
    let mut cached_env = env.clone();
    cached_env.push(("DISKTREE_DU_MAX_AGE", hour));
    let cached =
        run(du, &root, &[&args[..], &["--json"]].concat(), &cached_env);
    let want = if journal { "journal" } else { "directories" };
    assert_eq!(source_of(&cached), want, "{}", show(&cached));
    let cached = run(du, &root, &args, &cached_env);
    let walked = run(du, &root, &args, &[("DISKTREE_DU_INDEX", off)]);
    assert_eq!(
        String::from_utf8_lossy(&cached.stdout),
        String::from_utf8_lossy(&walked.stdout)
    );
}

#[test]
fn max_age_catches_up_by_directory() {
    catches_up(false);
}

#[cfg(target_os = "macos")]
#[test]
fn max_age_catches_up_through_the_journal() {
    catches_up(true);
}

/// On a case-insensitive volume an operand can be typed in another case
/// than the directory's; the journal still has to be matched against it.
#[cfg(target_os = "macos")]
#[test]
fn the_journal_is_matched_in_the_case_on_disk() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let root = temp.path().canonicalize().expect("canonical");
    write(&root.join("Proj/old"), 20_000);
    age(&root.join("Proj/old"), 7200);
    if !root.join("proj").exists() {
        eprintln!("SKIPPED: this volume is case-sensitive");
        return;
    }
    let index = root.join("index");
    let du = Path::new(env!("CARGO_BIN_EXE_disktree"));
    let hour = Path::new("1h");
    let env = [
        ("DISKTREE_DU_INDEX_DIR", index.as_path()),
        ("DISKTREE_DU_MAX_AGE", hour),
    ];
    run(du, &root, &["-s", "proj"], &env);
    std::fs::OpenOptions::new()
        .append(true)
        .open(root.join("Proj/old"))
        .and_then(|mut f| std::io::Write::write_all(&mut f, &vec![1; 300_000]))
        .expect("grow");
    std::thread::sleep(std::time::Duration::from_millis(500));
    let cached = run(du, &root, &["-s", "proj"], &env);
    let off = Path::new("0");
    let walked = run(du, &root, &["-s", "proj"], &[("DISKTREE_DU_INDEX", off)]);
    assert_eq!(
        String::from_utf8_lossy(&cached.stdout),
        String::from_utf8_lossy(&walked.stdout)
    );
}

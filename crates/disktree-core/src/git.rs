//! Running git to read a checkout, and only to read it.
//!
//! Every command runs with optional locks off, never prompts, never pages
//! and never reaches the network. A checkout's own config can name programs
//! for git to run, so each one a read could reach is overridden from the
//! command line, which outranks the repository's config. Nothing inherited
//! that points git elsewhere survives either: a program started from inside
//! a hook has `GIT_DIR` set.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// For every diff and patch read, so no driver the checkout configures turns
/// one into a program's output.
pub const DIFF_FLAGS: [&str; 5] = [
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-renames",
    "--full-index",
];

/// Long enough for `status` in a monorepo; a read that takes longer is
/// stopped rather than left holding a reader.
pub const TIMEOUT: Duration = Duration::from_secs(20);

/// What a command, or the last stage of a pipeline, said.
#[derive(Debug, Default)]
pub struct Output {
    /// The first failing stage's exit code; `None` when one could not start
    /// or was killed.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    /// The first line of the failing stage's error output.
    pub error: String,
    pub timed_out: bool,
}

impl Output {
    pub const fn ok(&self) -> bool {
        matches!(self.code, Some(0))
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn lines(&self) -> Vec<String> {
        self.text()
            .split('\n')
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// NUL-separated output, as `-z` writes it.
    pub fn fields(&self) -> Vec<String> {
        self.stdout
            .split(|&byte| byte == 0)
            .filter(|field| !field.is_empty())
            .map(|field| String::from_utf8_lossy(field).into_owned())
            .collect()
    }

    pub fn count(&self) -> Option<usize> {
        self.ok().then(|| self.text().trim().parse().ok()).flatten()
    }
}

#[derive(Clone, Debug)]
pub struct Git {
    executable: PathBuf,
    directory: PathBuf,
}

impl Git {
    /// `None` when there is no git to run.
    pub fn new(directory: &Path) -> Option<Self> {
        Some(Self {
            executable: executable()?.clone(),
            directory: directory.to_path_buf(),
        })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Global options for `status`, the one read an fsmonitor makes faster:
    /// git's own daemon, never a program the checkout names, and only one
    /// already running, since starting one for every checkout pointed at
    /// would leave processes behind.
    pub fn watched_status(&self) -> Vec<String> {
        if self.run(&["fsmonitor--daemon", "status"]).ok() {
            vec!["-c".into(), "core.fsmonitor=true".into()]
        } else {
            Vec::new()
        }
    }

    pub fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Output {
        self.pipeline(&[arguments(args)], TIMEOUT)
    }

    /// Each stage's output is the next one's input. The last stage's output
    /// comes back with the first failing stage's code, and everything still
    /// running at `timeout` is killed.
    pub fn pipeline(
        &self,
        stages: &[Vec<OsString>],
        timeout: Duration,
    ) -> Output {
        let mut children: Vec<Child> = Vec::with_capacity(stages.len());
        let mut errors: Vec<JoinHandle<Vec<u8>>> = Vec::new();
        let mut previous = None;
        for (index, args) in stages.iter().enumerate() {
            let mut command = self.command(args);
            command
                .stdin(previous.take().map_or_else(Stdio::null, Stdio::from))
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = match command.spawn() {
                Ok(child) => child,
                Err(error) => {
                    for mut started in children {
                        let _ = started.kill();
                        let _ = started.wait();
                    }
                    return Output {
                        error: error.to_string(),
                        ..Output::default()
                    };
                }
            };
            errors.push(drain(child.stderr.take()));
            if index + 1 < stages.len() {
                previous = child.stdout.take();
            }
            children.push(child);
        }
        // Read on its own thread, so a stage writing more than a pipe holds
        // never waits on a reader that is waiting on it.
        let stdout =
            drain(children.last_mut().and_then(|child| child.stdout.take()));

        let deadline = Instant::now() + timeout;
        let mut timed_out = false;
        while !children
            .iter_mut()
            .all(|child| !matches!(child.try_wait(), Ok(None)))
        {
            if Instant::now() >= deadline {
                timed_out = true;
                for child in &mut children {
                    let _ = child.kill();
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let codes: Vec<Option<i32>> = children
            .iter_mut()
            .map(|child| child.wait().ok().and_then(|status| status.code()))
            .collect();
        let stdout = stdout.join().unwrap_or_default();
        let errors: Vec<Vec<u8>> = errors
            .into_iter()
            .map(|handle| handle.join().unwrap_or_default())
            .collect();
        let failing = codes.iter().position(|code| *code != Some(0));
        let error = failing.map_or_else(String::new, |index| {
            String::from_utf8_lossy(&errors[index])
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
        });
        Output {
            code: failing.map_or(Some(0), |index| codes[index]),
            stdout,
            error,
            timed_out,
        }
    }

    fn command(&self, args: &[OsString]) -> Command {
        let mut command = Command::new(&self.executable);
        // disktree is a GUI program on Windows, with no console to lend:
        // without this, every read flashes a console window of its own.
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(
            &mut command,
            0x0800_0000, // CREATE_NO_WINDOW
        );
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        command
            .arg("-C")
            .arg(&self.directory)
            // An fsmonitor on every `status`, hooks, a pager, a signature
            // verifier, whatever a partial clone would run to fetch a
            // missing object: selecting a directory in a disk viewer must
            // not execute anything it contains, nor reach the network.
            // (From tobi/disktree#10.)
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.pager=cat",
                "-c",
                "log.showSignature=false",
                "-c",
                "gpg.program=false",
                "-c",
                "gpg.ssh.program=false",
                "-c",
                "gpg.x509.program=false",
                "-c",
                "core.sshCommand=false",
                "-c",
                "protocol.allow=never",
            ])
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_NO_LAZY_FETCH", "1")
            // A path like `app/[id]/page.tsx` is a path, never a pattern.
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("LC_ALL", "C");
        command
    }
}

pub fn arguments<S: AsRef<OsStr>>(args: &[S]) -> Vec<OsString> {
    args.iter().map(|arg| arg.as_ref().to_owned()).collect()
}

fn drain(
    reader: Option<impl std::io::Read + Send + 'static>,
) -> JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buffer = Vec::new();
        if let Some(mut reader) = reader {
            let _ = reader.read_to_end(&mut buffer);
        }
        buffer
    })
}

/// The filter drivers the checkout's own config defines that the user's own
/// config does not, by name; `None` when its config cannot be read.
///
/// `git status` runs a file's `clean` filter to compare it with the index,
/// and a downloaded repository can name any program as one in `.git/config`
/// and switch it on from its `.gitattributes`. Filters from the user's own
/// config, such as git-lfs, are theirs and still run, and so does a
/// checkout's copy of one, setting for setting, which is what `git lfs
/// install` writes into a repository. Anything else is switched off with
/// [`filters_off`]. Reading config runs nothing.
pub fn foreign_filters(git: &Git) -> Option<Vec<String>> {
    let output = git.run(&[
        "config",
        "--show-scope",
        "--includes",
        "--get-regexp",
        r"^filter\.",
    ]);
    match output.code {
        // No filter anywhere.
        Some(1) => Some(Vec::new()),
        Some(0) => Some(foreign_filter_names(&output.text())),
        _ => None,
    }
}

/// From `config --show-scope --get-regexp` lines: `scope<TAB>key value`,
/// where a driver's name may itself hold dots.
fn foreign_filter_names(listing: &str) -> Vec<String> {
    let settings: Vec<(&str, &str, &str, &str)> = listing
        .lines()
        .filter_map(|line| {
            let (scope, setting) = line.split_once('\t')?;
            let (key, value) = setting.split_once(' ').unwrap_or((setting, ""));
            let (name, variable) =
                key.strip_prefix("filter.")?.rsplit_once('.')?;
            Some((scope, name, variable, value))
        })
        .collect();
    let mut foreign: Vec<String> = Vec::new();
    for &(scope, name, variable, value) in &settings {
        if !matches!(scope, "local" | "worktree") {
            continue;
        }
        let users_own =
            settings
                .iter()
                .any(|&(own, own_name, own_variable, own_value)| {
                    matches!(own, "system" | "global")
                        && (own_name, own_variable, own_value)
                            == (name, variable, value)
                });
        if !users_own && !foreign.iter().any(|known| known == name) {
            foreign.push(name.to_owned());
        }
    }
    foreign
}

/// Global options that switch the named filter drivers off for one
/// command.
///
/// With no command left, git compares a file as it is on disk, and a driver
/// marked required is not missed. At worst a file its filter would have
/// cleaned reads as changed, which only ever says there is more to lose.
pub fn filters_off(names: &[String]) -> Vec<String> {
    names
        .iter()
        .flat_map(|name| {
            [
                format!("filter.{name}.clean="),
                format!("filter.{name}.smudge="),
                format!("filter.{name}.process="),
                format!("filter.{name}.required=false"),
            ]
        })
        .flat_map(|setting| ["-c".to_owned(), setting])
        .collect()
}

/// The git to run, looked for once.
///
/// On macOS `/usr/bin/git` is there even without the developer tools, as a
/// stub that opens an "install the command line developer tools" dialog,
/// which reading a checkout must not do; so a real git is looked for where
/// Homebrew and the developer directories put one. Elsewhere, `git` on the
/// `PATH`.
pub fn executable() -> Option<&'static PathBuf> {
    static FOUND: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOUND
        .get_or_init(|| {
            if cfg!(target_os = "macos") {
                let developer = [
                    std::env::var_os("DEVELOPER_DIR").map(PathBuf::from),
                    std::fs::read_link("/var/db/xcode_select_link").ok(),
                    Some("/Library/Developer/CommandLineTools".into()),
                    Some("/Applications/Xcode.app/Contents/Developer".into()),
                ];
                ["/opt/homebrew/bin/git", "/usr/local/bin/git"]
                    .into_iter()
                    .map(PathBuf::from)
                    .chain(
                        developer
                            .into_iter()
                            .flatten()
                            .map(|directory| directory.join("usr/bin/git")),
                    )
                    .find(|path| is_executable(path))
            } else {
                let name = if cfg!(windows) { "git.exe" } else { "git" };
                std::env::split_paths(&std::env::var_os("PATH")?)
                    .map(|directory| directory.join(name))
                    .find(|path| is_executable(path))
            }
        })
        .as_ref()
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pipeline_feeds_each_stage_the_one_before() {
        let Some(git) = Git::new(Path::new(".")) else {
            return; // no git on this machine
        };
        let output = git.pipeline(
            &[
                arguments(&["hash-object", "--stdin"]),
                arguments(&["hash-object", "--stdin"]),
            ],
            TIMEOUT,
        );
        assert!(output.ok(), "{}", output.error);
        assert_eq!(output.text().trim().len(), 40);
    }

    #[test]
    fn only_a_filter_the_user_does_not_define_is_foreign() {
        let lfs = "filter.lfs.clean git-lfs clean -- %f\n\
                   filter.lfs.process git-lfs filter-process\n\
                   filter.lfs.required true";
        let scoped = |scope: &str| {
            lfs.lines()
                .map(|line| [scope, "\t", line, "\n"].concat())
                .collect::<String>()
        };
        let copied = format!("{}{}", scoped("global"), scoped("local"));
        assert!(
            foreign_filter_names(&copied).is_empty(),
            "`git lfs install` copies the user's own filter into a repository"
        );
        assert_eq!(foreign_filter_names(&scoped("local")), ["lfs"]);
        let changed = format!(
            "{}local\tfilter.lfs.process ./fetch.sh\n",
            scoped("global")
        );
        assert_eq!(
            foreign_filter_names(&changed),
            ["lfs"],
            "one setting of its own makes the whole driver the checkout's"
        );
        assert_eq!(
            foreign_filter_names("worktree\tfilter.a.b.clean ./a.sh\n"),
            ["a.b"]
        );
        assert_eq!(
            filters_off(&["x".into()]),
            [
                "-c",
                "filter.x.clean=",
                "-c",
                "filter.x.smudge=",
                "-c",
                "filter.x.process=",
                "-c",
                "filter.x.required=false"
            ]
        );
    }

    #[test]
    fn a_failing_stage_is_reported_with_what_it_said() {
        let Some(git) = Git::new(Path::new(".")) else {
            return; // no git on this machine
        };
        let output = git.run(&["rev-parse", "--verify", "-q", "no-such-ref"]);
        assert!(!output.ok());
        let output = git.run(&["not-a-command"]);
        assert!(!output.ok());
        assert!(!output.error.is_empty(), "git says what went wrong");
    }
}

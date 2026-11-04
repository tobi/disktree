//! Bounded, cancellable direct process invocation. Never invokes a shell or
//! downloads a missing CLI. Full output is drained but only a prefix retained.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command as Process, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

use super::{Command, Environment, Issue, Operation, Owner, Probe, Problem};

const OUTPUT_LIMIT: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Identity {
    length: u64,
    modified: SystemTime,
}

impl Identity {
    pub(super) fn read(path: &Path) -> Result<Self, String> {
        let meta = std::fs::metadata(path).map_err(|error| {
            format!("Owner executable unavailable: {error}")
        })?;
        if !meta.is_file() {
            return Err("Owner executable is not a file".into());
        }
        Ok(Self {
            length: meta.len(),
            modified: meta.modified().map_err(|error| error.to_string())?,
        })
    }
}

pub(super) struct SystemProbe;

impl Probe for SystemProbe {
    fn executable(
        &mut self,
        name: &str,
        env: &Environment,
    ) -> Result<PathBuf, Issue> {
        resolve_executable(name, env, 0)
    }

    fn output(
        &mut self,
        command: &Command,
        env: &Environment,
    ) -> Result<String, String> {
        let output = run(
            command,
            env,
            &AtomicBool::new(false),
            Duration::from_secs(10),
        )
        .map_err(|issue| issue.detail)?;
        if output.len() as u64 >= OUTPUT_LIMIT {
            Err("Owner probe output exceeded its bound".into())
        } else {
            String::from_utf8(output).map_err(|_| "Owner probe returned non-UTF-8 output; cannot resolve an exact cache root".into())
        }
    }
}

#[allow(
    clippy::unnecessary_debug_formatting,
    reason = "Executable refusals must expose path controls"
)]
fn resolve_executable(
    name: &str,
    env: &Environment,
    depth: usize,
) -> Result<PathBuf, Issue> {
    let launcher =
        |detail: &str| Issue::new(Problem::UnsupportedLauncher, detail);
    if depth > 4 {
        return Err(launcher(
            "Unsupported recursive CLI interpreter; expose the installed executable directly",
        ));
    }
    // Windows .cmd launchers need a shell and have different quoting
    // rules. Do not quietly run them through cmd.exe.
    if cfg!(windows) {
        return Err(launcher(
            "Owner-command execution is currently supported on macOS and Linux; Windows launchers require a separate audited adapter",
        ));
    }
    let candidates = if name == "pip" {
        vec!["pip", "pip3"]
    } else {
        vec![name]
    };
    for candidate in candidates {
        for dir in std::env::split_paths(
            env.variables.get("PATH").map_or("", String::as_str),
        ) {
            // Relative PATH entries could resolve a different project CLI.
            if !dir.is_absolute() {
                continue;
            }
            let path = dir.join(candidate);
            if path.is_file() {
                let resolved =
                    std::fs::canonicalize(&path).map_err(|error| {
                        Issue::new(Problem::NotFound, error.to_string())
                    })?;
                if is_shim(&path) || is_shim(&resolved) {
                    return Err(launcher(
                        "Version-manager shim unavailable for cleanup; expose the installed executable directly on PATH. PwrDisk does not auto-install missing tools",
                    ));
                }
                // npm's distributed CLI resolves to npm-cli.js. A proxy
                // such as Corepack can download npm even for --version.
                if name == "npm"
                    && resolved
                        .file_name()
                        .is_none_or(|name| name != "npm-cli.js")
                {
                    return Err(launcher(
                        "npm proxy/wrapper launchers are unsupported; expose the installed npm-cli.js through its npm symlink on PATH",
                    ));
                }
                validate_interpreter(&resolved, env, depth)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    if std::fs::metadata(&path)
                        .map_err(|error| {
                            Issue::new(Problem::NotFound, error.to_string())
                        })?
                        .permissions()
                        .mode()
                        & 0o111
                        == 0
                    {
                        return Err(Issue::new(
                            Problem::NotFound,
                            format!(
                                "Executable found but not executable: {path:?}"
                            ),
                        ));
                    }
                }
                return Ok(resolved);
            }
        }
    }
    Err(Issue::new(
        Problem::NotFound,
        format!(
            "{name} command unavailable on PwrDisk's PATH (software may still be installed)"
        ),
    ))
}

fn is_shim(path: &Path) -> bool {
    path.components().any(|part| part.as_os_str() == "shims")
        || path.file_name().is_some_and(|name| name == "volta-shim")
        || path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "bin")
            && path
                .parent()
                .and_then(Path::parent)
                .and_then(Path::file_name)
                .is_some_and(|name| name == ".volta")
}

fn validate_interpreter(
    path: &Path,
    env: &Environment,
    depth: usize,
) -> Result<(), Issue> {
    let launcher =
        |detail: &str| Issue::new(Problem::UnsupportedLauncher, detail);
    // An npm or Cypress script can use env's node, which must not silently
    // dispatch through a version manager that installs a runtime on demand.
    let mut prefix = [0; 512];
    let length = std::fs::File::open(path)
        .and_then(|mut file| file.read(&mut prefix))
        .map_err(|error| Issue::new(Problem::NotFound, error.to_string()))?;
    let header = String::from_utf8_lossy(&prefix[..length]);
    let Some(shebang) = header
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("#!"))
    else {
        return Ok(());
    };
    let words: Vec<_> = shebang.split_whitespace().collect();
    if words.first().is_some_and(|program| {
        Path::new(program)
            .file_name()
            .is_some_and(|name| name == "env")
    }) {
        let [_, runtime] = words.as_slice() else {
            return Err(launcher(
                "Unsupported env interpreter arguments; expose a direct installed CLI",
            ));
        };
        resolve_executable(runtime, env, depth + 1)?;
    } else if words
        .first()
        .is_some_and(|program| is_shim(Path::new(program)))
    {
        return Err(launcher(
            "CLI interpreter is a version-manager shim; expose the installed runtime directly",
        ));
    }
    Ok(())
}

pub(super) fn run(
    command: &Command,
    env: &Environment,
    cancel: &AtomicBool,
    timeout: Duration,
) -> Result<Vec<u8>, Issue> {
    let failed = |detail: String| Issue::new(Problem::CommandFailed, detail);
    if cancel.load(Ordering::Relaxed) {
        return Err(Issue::new(
            Problem::Cancelled,
            "Cancelled before command start",
        ));
    }
    let mut process = Process::new(&command.executable);
    process
        .args(&command.arguments)
        .env_clear()
        .envs(&env.variables)
        .envs(&command.environment)
        .current_dir(&command.working_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Kill a subprocess group on cancellation, so a launcher cannot leave
    // a cleaning child behind. Every supported adapter is Unix-only.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        process.process_group(0);
    }
    let mut child = process.spawn().map_err(|error| {
        Issue::new(
            Problem::NotFound,
            format!("Owner command could not start: {error}"),
        )
    })?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| failed("Missing owner stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| failed("Missing owner stderr".into()))?;
    let read = |mut pipe: Box<dyn std::io::Read + Send>| {
        let mut bytes = Vec::new();
        let result = (&mut pipe).take(OUTPUT_LIMIT).read_to_end(&mut bytes);
        let _ = std::io::copy(&mut pipe, &mut std::io::sink());
        result.map(|_| bytes)
    };
    let (out_send, out_recv) = mpsc::channel();
    let (err_send, err_recv) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = out_send.send(read(Box::new(stdout)));
    });
    std::thread::spawn(move || {
        let _ = err_send.send(read(Box::new(stderr)));
    });
    let start = Instant::now();
    let mut status = None;
    let mut output = None;
    let mut errors = None;
    loop {
        if cancel.load(Ordering::Relaxed) || start.elapsed() >= timeout {
            terminate(&mut child);
            return Err(Issue::new(
                Problem::Cancelled,
                "Owner command cancelled or timed out; it may have partially cleaned the cache. Nothing else was deleted; inspect and preview again",
            ));
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(value) => status = value,
                Err(error) => {
                    terminate(&mut child);
                    return Err(failed(format!(
                        "Cannot observe owner command: {error}"
                    )));
                }
            }
        }
        if output.is_none() {
            output = out_recv.try_recv().ok();
        }
        if errors.is_none() {
            errors = err_recv.try_recv().ok();
        }
        if let (Some(status), Some(output), Some(errors)) =
            (status, output.as_ref(), errors.as_ref())
        {
            let errors =
                errors.as_ref().map_err(|error| failed(error.to_string()))?;
            if !status.success() {
                return Err(failed(format!(
                    "Owner command failed ({status}): {}. It may have partially cleaned the cache. Nothing else was deleted",
                    String::from_utf8_lossy(errors).escape_debug()
                )));
            }
            let output =
                output.as_ref().map_err(|error| failed(error.to_string()))?;
            return Ok(output.clone());
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

fn terminate(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id())
        && let Some(pid) = rustix::process::Pid::from_raw(pid)
    {
        let _ = rustix::process::kill_process_group(
            pid,
            rustix::process::Signal::KILL,
        );
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Owners can follow links internally. Refuse any link or volume boundary
/// inside their actual scope, rather than assuming their traversal matches
/// PwrDisk's unlink semantics. Bound the walk and fail closed on read errors.
#[allow(
    clippy::unnecessary_debug_formatting,
    reason = "Safety errors must expose path controls"
)]
pub(super) fn check_tree(root: &Path) -> Result<(), String> {
    let volume =
        crate::space::device_for(root).ok_or("Cannot identify cache volume")?;
    let mut pending = vec![root.to_owned()];
    let mut visited = 0;
    while let Some(path) = pending.pop() {
        visited += 1;
        if visited > super::MAX_PREFLIGHT_ENTRIES {
            return Err("Cache is too large for bounded owner preflight; inspect manually".into());
        }
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|error| error.to_string())?;
        if meta.is_symlink() {
            if path == root {
                return Err(
                    "Owner cache scope is a symlink; inspect its actual root"
                        .into(),
                );
            }
            let destination =
                std::fs::canonicalize(&path).map_err(|error| {
                    format!("Cannot resolve cache symlink: {error}")
                })?;
            let real_root = std::fs::canonicalize(root)
                .map_err(|error| error.to_string())?;
            if !destination.starts_with(real_root) {
                return Err(format!(
                    "Owner scope contains an external symlink: {path:?}; inspect manually"
                ));
            }
        }
        if meta.is_dir() {
            if crate::space::device_for(&path).as_deref()
                != Some(volume.as_str())
            {
                return Err("Owner scope crosses a volume boundary".into());
            }
            for entry in
                std::fs::read_dir(&path).map_err(|error| error.to_string())?
            {
                if visited + pending.len() >= super::MAX_PREFLIGHT_ENTRIES {
                    return Err("Cache is too large for bounded owner preflight; inspect manually".into());
                }
                pending.push(entry.map_err(|error| error.to_string())?.path());
            }
        }
    }
    Ok(())
}

/// Whether [`check_in_use`] can look at running processes here.
pub(super) const ACTIVE_USE_CHECKED: bool =
    cfg!(any(target_os = "macos", target_os = "linux"));

pub(super) fn check_active(operation: &Operation) -> Result<(), String> {
    check_in_use(
        &operation.scope,
        &operation.cache_root,
        Some(operation.owner),
        Some(&operation.command.executable),
    )
}

/// Deleting files directly has no owner command, but the same live writers
/// matter: a process running from or inside the path, one naming it, and
/// for a recognized cache any running instance of its owner.
pub(super) fn check_path_active(
    path: &Path,
    owner: Option<Owner>,
) -> Result<(), String> {
    check_in_use(path, path, owner, None)
}

fn check_in_use(
    scope: &Path,
    cache: &Path,
    owner: Option<Owner>,
    executable: Option<&Path>,
) -> Result<(), String> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mut system = sysinfo::System::new();
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            sysinfo::ProcessRefreshKind::everything(),
        );
        if system.processes().is_empty() {
            return Err("Cannot check active processes".into());
        }
        let cache = cache.to_str().ok_or("Unsupported cache path")?;
        for (pid, process) in system.processes() {
            if pid.as_u32() == std::process::id() {
                continue;
            }
            let in_scope = process
                .exe()
                .is_some_and(|path| path.starts_with(scope))
                || process.cwd().is_some_and(|path| path.starts_with(scope));
            let using_cache = process.cmd().iter().any(|argument| {
                let argument = argument.to_string_lossy();
                argument.contains(cache)
                    || owner.is_some_and(|owner| {
                        owner_named(owner, executable, &argument)
                    })
            });
            if in_scope || using_cache {
                return Err(format!(
                    "{} may be in use (process {pid}); stop it and prepare a new preview",
                    owner.map_or("This path", Owner::label)
                ));
            }
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (scope, cache, owner, executable);
        Err("Active-use checking is unsupported on this platform".into())
    }
}

/// Node launchers appear as `node .../npm-cli.js`, and Python installations
/// commonly expose pip3. Comparing only the friendly owner name misses
/// those live writers even though their configured cache is the same.
#[cfg(test)]
pub(super) fn owner_argument(operation: &Operation, argument: &str) -> bool {
    owner_named(
        operation.owner,
        Some(&operation.command.executable),
        argument,
    )
}

#[cfg(any(target_os = "macos", target_os = "linux", test))]
fn owner_named(
    owner: Owner,
    executable: Option<&Path>,
    argument: &str,
) -> bool {
    let path = Path::new(argument);
    let name = path.file_name();
    executable.is_some_and(|executable| path == executable)
        || name.is_some_and(|name| {
            name == owner.executable()
                || executable.is_some_and(|executable| {
                    Some(name) == executable.file_name()
                })
                || (matches!(owner, Owner::Npm | Owner::Npx)
                    && matches!(
                        name.to_str(),
                        Some("npm" | "npx" | "npm-cli.js" | "npx-cli.js")
                    ))
                || (owner == Owner::Pip && name == "pip3")
        })
}

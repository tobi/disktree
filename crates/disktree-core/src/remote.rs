//! Read-only, versioned SSH scan protocol.
//!
//! Paths travel as JSON on stdin,
//! never as shell arguments. Postorder records avoid JSON recursion limits
//! and stream without making a second serialized copy of the whole tree.

use std::io::{self, BufRead, BufReader, Read as _, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};

use serde::{Deserialize, Serialize};

use crate::path_style::PathStyle;
use crate::remote_progress::{INTERVAL, Progress, Shared, Stage, stage};
use crate::scan::ScanOptions;
#[cfg(not(unix))]
use crate::scan::scan;
use crate::space::{SpaceInfo, space_info, volume_root_for};
use crate::tree::{Node, NodeKind, aggregate};
use std::time::Instant;

const VERSION: u32 = 1;
const MAX_LINE: u64 = 1 << 20;
const MAX_DIAGNOSTICS: usize = 16 << 10;

/// Whether `path` names a directory from the remote host's root. Hosts are
/// Unix, so this is POSIX on every client; see `path_style`.
#[must_use]
pub fn is_remote_absolute(path: &Path) -> bool {
    PathStyle::Posix.is_absolute(path)
}

/// The remote command, the scanners and the delete script are POSIX shell
/// and Linux tools, and remote paths are joined as POSIX paths. A Windows
/// host's `C:\` paths would be joined wrongly, so refuse it by name.
fn check_platform(platform: &str) -> io::Result<()> {
    if platform == "windows" {
        return Err(io::Error::other(
            "SSH hosts must run Unix; Windows hosts are not supported",
        ));
    }
    Ok(())
}

// Non-interactive shells often omit ~/.local/bin. Do not require users to
// edit the server's startup files after the documented user-local install.
const REMOTE_COMMAND: &str = r#"if command -v disktree-scan >/dev/null 2>&1; then
exec disktree-scan --stdio-v1
elif [ -x "$HOME/.local/bin/disktree-scan" ]; then
exec "$HOME/.local/bin/disktree-scan" --stdio-v1
else
echo 'disktree-scan is not installed on this server.' >&2
echo 'On the server, run make install-scan from the disktree checkout.' >&2
exit 127
fi"#;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub root: Option<PathBuf>,
    pub disk: bool,
    pub options: ScanOptions,
}

// Old helpers ignore this extra field. Old clients may still half-close
// stdin; only clients opting in promise to keep it open during the scan.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Operation {
    // No scan fields: legacy helpers reject discovery before any traversal,
    // instead of silently ignoring an unfamiliar operation and scanning ~.
    Discover { discover_version: u32 },
    Scan(Request),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionRequest {
    #[serde(flatten)]
    pub operation: Operation,
    #[serde(default)]
    pub watch_disconnect: bool,
    // Opt-in keeps the postorder-only wire format for older clients.
    #[serde(default)]
    pub live_progress: bool,
    #[serde(default)]
    pub client_dedup: bool,
    #[serde(default)]
    pub live_tree: bool,
}

/// The target's root candidates, without walking or probing any of them.
#[derive(Debug, Serialize, Deserialize)]
pub struct Roots {
    version: u32,
    pub home: Option<PathBuf>,
    pub platform: String,
    pub identity: Option<crate::identity::HostIdentity>,
    pub volumes: Vec<crate::space::Volume>,
}

pub fn read_roots(input: &mut impl BufRead) -> io::Result<Roots> {
    let roots: Roots = receive(input)?;
    check_platform(&roots.platform)?;
    if roots.version != VERSION
        || roots
            .volumes
            .iter()
            .any(|volume| !is_remote_absolute(&volume.point))
        || roots
            .home
            .as_ref()
            .is_some_and(|home| !is_remote_absolute(home))
    {
        return Err(io::Error::other("incompatible remote root discovery"));
    }
    Ok(roots)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Header {
    version: u32,
    pub root: PathBuf,
    pub home: Option<PathBuf>,
    pub disk_root: Option<PathBuf>,
    pub space: Option<SpaceInfo>,
    pub platform: String,
    #[serde(default)]
    pub identity: Option<crate::identity::HostIdentity>,
}

#[derive(Debug)]
pub struct Snapshot {
    pub header: Header,
    pub tree: Node,
}

// Children are already on the receiver's stack; counts preserve empty dirs
// and deduplicated leaves without transmitting derived subtree totals.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Record(
    pub String,
    pub u8,
    pub u64,
    pub u64,
    pub i64,
    pub bool,
    pub usize,
);

#[derive(Deserialize)]
struct Control {
    directory: Option<PathBuf>,
    enter: Option<String>,
    entry: Option<Record>,
    link: Option<(u64, u64)>,
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

pub(crate) fn send(
    out: &mut impl Write,
    value: &impl Serialize,
) -> io::Result<()> {
    serde_json::to_writer(&mut *out, value).map_err(json_error)?;
    out.write_all(b"\n")
}

fn receive<T: serde::de::DeserializeOwned>(
    input: &mut impl BufRead,
) -> io::Result<T> {
    let mut line = String::new();
    receive_line(input, &mut line)?;
    serde_json::from_str(&line).map_err(json_error)
}

fn receive_line(input: &mut impl BufRead, line: &mut String) -> io::Result<()> {
    line.clear();
    // A remote process is not trusted to allocate arbitrary amounts of RAM
    // with a single malformed record (or an accidental login banner).
    let length = input.take(MAX_LINE).read_line(line)?;
    if length == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "remote helper closed the connection without a complete scan",
        ));
    }
    if !line.ends_with('\n') {
        let reason = if length as u64 == MAX_LINE {
            "oversized remote record"
        } else {
            "truncated remote record"
        };
        return Err(io::Error::new(io::ErrorKind::InvalidData, reason));
    }
    Ok(())
}

/// Only read-only discovery and scans: no command execution or removal.
pub fn serve(input: &mut impl BufRead, out: &mut impl Write) -> io::Result<()> {
    serve_session(read_request(input)?, out)
}

/// Read one bounded request, including the opt-in connection lifetime.
pub fn read_request(input: &mut impl BufRead) -> io::Result<SessionRequest> {
    receive(input)
}

/// Dispatch without resolving any scan path until a scan was requested.
pub fn serve_session(
    session: SessionRequest,
    out: &mut impl Write,
) -> io::Result<()> {
    match session.operation {
        Operation::Scan(request) => serve_request_with_progress(
            request,
            out,
            session.live_progress,
            session.client_dedup,
            session.live_tree,
        ),
        Operation::Discover { discover_version } => {
            if discover_version != VERSION {
                return Err(io::Error::other(
                    "unsupported root discovery version",
                ));
            }
            send(
                out,
                &Roots {
                    version: VERSION,
                    home: std::env::home_dir(),
                    platform: std::env::consts::OS.into(),
                    identity: crate::identity::HostIdentity::current(),
                    volumes: crate::space::volume_roots()?,
                },
            )?;
            out.flush()
        }
    }
}

/// Scan a decoded request. The standalone helper supervises its lifetime.
pub fn serve_request(request: Request, out: &mut impl Write) -> io::Result<()> {
    serve_request_with_progress(request, out, false, false, false)
}

fn serve_request_with_progress(
    request: Request,
    out: &mut impl Write,
    live_progress: bool,
    client_dedup: bool,
    live_tree: bool,
) -> io::Result<()> {
    let _ = (live_progress, client_dedup, live_tree); // Only Unix has the streaming walker.
    let home = std::env::home_dir();
    let root = request
        .root
        .or_else(|| home.clone())
        .ok_or_else(|| io::Error::other("no remote home directory"))?;
    let root = if request.disk {
        volume_root_for(&root).unwrap_or_else(|| PathBuf::from("/"))
    } else {
        root
    }
    .canonicalize()?;
    if !root.is_dir() {
        return Err(io::Error::other("scan root is not a directory"));
    }
    send(
        out,
        &Header {
            version: VERSION,
            disk_root: volume_root_for(&root),
            space: space_info(&root).ok(),
            root: root.clone(),
            home,
            platform: std::env::consts::OS.into(),
            identity: crate::identity::HostIdentity::current(),
        },
    )?;
    out.flush()?;
    #[cfg(unix)]
    crate::remote_walk::stream(
        &root,
        &request.options,
        out,
        live_progress,
        client_dedup,
        live_tree,
    )?;
    #[cfg(not(unix))]
    write_node(out, &scan(&root, request.options)?)?;
    // An explicit terminator distinguishes a complete scan from a severed SSH
    // connection after any otherwise valid prefix of the postorder stream.
    send(out, &"done")?;
    out.flush()
}

#[cfg(not(unix))]
fn write_node(out: &mut impl Write, node: &Node) -> io::Result<()> {
    for child in &node.children {
        write_node(out, child)?;
    }
    let kind = match node.kind {
        NodeKind::Directory => 0,
        NodeKind::File => 1,
        NodeKind::Symlink => 2,
        NodeKind::Other => 3,
    };
    send(
        out,
        &Record(
            if node.name.contains('/') {
                "root".into()
            } else {
                node.name.to_string()
            },
            kind,
            node.own_bytes(),
            node.own_files(),
            node.modified,
            node.read_error,
            node.children.len(),
        ),
    )
}

pub fn read_snapshot(input: &mut impl BufRead) -> io::Result<Snapshot> {
    read_snapshot_with_progress(input, &Shared::default())
}

fn read_snapshot_with_progress(
    input: &mut impl BufRead,
    shared: &Shared,
) -> io::Result<Snapshot> {
    let header: Header = receive(input)?;
    check_platform(&header.platform)?;
    if header.version != VERSION
        || !is_remote_absolute(&header.root)
        || [&header.home, &header.disk_root]
            .into_iter()
            .flatten()
            .any(|path| !is_remote_absolute(path))
    {
        return Err(io::Error::other("incompatible remote helper protocol"));
    }
    let mut progress = Progress {
        stage: Stage::Indexing,
        directory: Some(header.root.clone()),
        updated: Some(Instant::now()),
        ..Progress::default()
    };
    *shared.lock().expect("remote progress lock") = progress.clone();
    let mut published = Instant::now();
    let mut stack = Vec::<Node>::new();
    let mut line = String::new();
    let seen = crate::tree::Seen::new();
    let mut preview = crate::scan_preview::Builder::default();
    let mut ancestors = Vec::<String>::new();
    let mut preview_published = Instant::now();
    loop {
        receive_line(input, &mut line)?;
        let text = line.trim_start();
        // The common path is a typed tuple, not a heap-allocated JSON DOM
        // followed by another deserialization. Reuse the line allocation.
        let record: Record = if text.starts_with('[') {
            serde_json::from_str(text).map_err(json_error)?
        } else if text.starts_with('{') {
            let control: Control =
                serde_json::from_str(text).map_err(json_error)?;
            if let Some(name) = control.enter {
                if !(name.is_empty() && ancestors.is_empty())
                    && (name.is_empty()
                        || name == "."
                        || name == ".."
                        || name.contains(['/', '\0']))
                {
                    return Err(io::Error::other("invalid preview directory"));
                }
                ancestors.push(name);
                continue;
            }
            if let Some(directory) = control.directory {
                progress.directory = Some(directory);
                progress.updated = Some(Instant::now());
                *shared.lock().expect("remote progress lock") =
                    progress.clone();
                published = Instant::now();
                continue;
            }
            let mut entry = control.entry.ok_or_else(|| {
                io::Error::other("invalid remote control record")
            })?;
            let identity = control.link.ok_or_else(|| {
                io::Error::other("missing remote hardlink identity")
            })?;
            if entry.1 != 1 || entry.6 != 0 {
                return Err(io::Error::other("invalid remote hardlink entry"));
            }
            if entry.2 > 0 && !seen.insert(identity) {
                entry.2 = 0;
            }
            entry
        } else if serde_json::from_str::<String>(text).map_err(json_error)?
            == "done"
        {
            break;
        } else {
            return Err(io::Error::other("invalid remote stream marker"));
        };
        let Record(name, kind, bytes, files, modified, read_error, count) =
            record;
        // Names must remain single components when joined to marked paths.
        // Remote paths are joined as POSIX paths on every client, so `\` and
        // `:` are ordinary characters in a name, as they are on the host.
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.contains(['/', '\0'])
        {
            return Err(io::Error::other(format!(
                "remote name cannot be represented safely: {name:?}"
            )));
        }
        if count > stack.len() {
            return Err(io::Error::other(format!(
                "incomplete remote directory {name:?}: \
                 expected {count} children, received {}",
                stack.len(),
            )));
        }
        let kind = match kind {
            0 => NodeKind::Directory,
            1 => NodeKind::File,
            2 => NodeKind::Symlink,
            3 => NodeKind::Other,
            _ => return Err(io::Error::other("invalid remote node kind")),
        };
        if !kind.is_dir() && count != 0 {
            return Err(io::Error::other("remote leaf has children"));
        }
        // The final root often owns every stacked node. Transfer that
        // allocation instead of briefly duplicating a wide tree's storage.
        let children = if count == stack.len() {
            std::mem::take(&mut stack)
        } else {
            stack.split_off(stack.len() - count)
        };
        let mut node = Node::entry(name, kind, bytes);
        node.files = files;
        node.modified = modified;
        node.read_error = read_error;
        node.children = children;
        stack.push(node);
        let mut preview_changed = false;
        if !ancestors.is_empty() {
            if kind.is_dir() {
                ancestors.pop();
            } else {
                preview.add(ancestors.get(1).map(String::as_str), bytes, files);
                if progress.preview.is_none() {
                    progress.preview = Some(Arc::new(preview.snapshot()));
                    preview_changed = true;
                }
            }
        }
        progress.dirs += u64::from(kind.is_dir());
        // Legacy helpers include direct totals in directory records too.
        // Count each leaf once, never charge its parent's totals again.
        if !kind.is_dir() {
            progress.files += files;
            progress.bytes += bytes;
        }
        progress.errors += u64::from(read_error);
        if published.elapsed() >= INTERVAL || preview_changed {
            if progress.preview.is_some()
                && preview_published.elapsed().as_secs() >= 1
            {
                progress.preview = Some(Arc::new(preview.snapshot()));
                preview_published = Instant::now();
            }
            progress.updated = Some(Instant::now());
            *shared.lock().expect("remote progress lock") = progress.clone();
            published = Instant::now();
        }
    }
    if progress.preview.is_some() {
        progress.preview = Some(Arc::new(preview.snapshot()));
    }
    progress.stage = Stage::Finalizing;
    progress.updated = Some(Instant::now());
    *shared.lock().expect("remote progress lock") = progress;
    if stack.len() != 1 {
        return Err(io::Error::other("incomplete remote tree"));
    }
    let mut tree = stack.pop().expect("one root checked above");
    aggregate(&mut tree, crate::tree::Metric::Bytes);
    crate::classify::classify(&mut tree);
    Ok(Snapshot { header, tree })
}

pub fn validate_host(host: &str) -> io::Result<()> {
    if host.is_empty()
        || host.starts_with('-')
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._-:[]".contains(c))
    {
        return Err(io::Error::other("invalid SSH destination"));
    }
    Ok(())
}

fn read_diagnostics(mut input: impl io::Read) -> String {
    let mut tail = Vec::new();
    let mut chunk = [0_u8; 4096];
    while let Ok(length) = input.read(&mut chunk) {
        if length == 0 {
            break;
        }
        tail.extend_from_slice(&chunk[..length]);
        if tail.len() > MAX_DIAGNOSTICS {
            tail.drain(..tail.len() - MAX_DIAGNOSTICS);
        }
    }
    // Diagnostics are remote-controlled too; keep text, not terminal escapes.
    String::from_utf8_lossy(&tail)
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .collect::<String>()
        .trim()
        .to_owned()
}

fn ssh_error(host: &str, details: &str, fallback: &io::Error) -> io::Error {
    let reason = if details.is_empty() {
        fallback.to_string()
    } else {
        details.to_owned()
    };
    let hint = if reason.contains("Permission denied")
        || reason.contains("Authentication failed")
    {
        format!(
            "Run ssh {host} in a terminal to verify your credentials. \
             When starting disktree from a terminal, SSH can ask for a \
             password there; GUI launches need an agent or SSH askpass."
        )
    } else if reason.contains("Host key verification failed") {
        format!("Run ssh {host} in a terminal and verify its host key first.")
    } else if reason.contains("not installed") || reason.contains("not found") {
        format!(
            "Install disktree-scan ON {host}, not on this computer. \
             Run make install-scan from the checkout on the server."
        )
    } else {
        "No incomplete scan is shown. Retry, or choose another root.".into()
    };
    io::Error::other(format!("SSH {host}: {reason}\n\n{hint}"))
}

/// Cancelling a rescan or closing a window kills SSH and closes the pipe.
///
/// The helper monitors stdin for EOF even before it has a tree to write;
/// keep the request pipe open until the entire snapshot has arrived.
#[derive(Debug)]
pub struct RemoteJob<T> {
    child: Arc<Mutex<Child>>,
    result: mpsc::Receiver<io::Result<T>>,
    progress: Shared,
}

pub type RemoteScan = RemoteJob<Snapshot>;
pub type RemoteRoots = RemoteJob<Roots>;

fn ssh_command(
    host: &str,
) -> io::Result<(Command, Option<crate::payload::Archive>)> {
    validate_host(host)?;
    let archive = crate::payload::Archive::current()?;
    let remote = if archive.is_some() {
        crate::deployment::COMMAND
    } else {
        REMOTE_COMMAND
    };
    let mut command = Command::new("ssh");
    command.args([
        "-T",
        "-C",
        "-o",
        "ConnectTimeout=15",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=2",
        "--",
        host,
        remote,
    ]);
    Ok((command, archive))
}

impl RemoteScan {
    pub fn spawn(host: &str, request: &Request) -> io::Result<Self> {
        let (command, archive) = ssh_command(host)?;
        Self::spawn_with_archive(command, host, request, archive)
    }

    // Only the Unix tests run a fake ssh, so Windows has no caller.
    #[cfg(all(test, unix))]
    fn spawn_command(
        command: Command,
        host: &str,
        request: &Request,
    ) -> io::Result<Self> {
        Self::spawn_with_archive(command, host, request, None)
    }

    fn spawn_with_archive(
        command: Command,
        host: &str,
        request: &Request,
        archive: Option<crate::payload::Archive>,
    ) -> io::Result<Self> {
        let metric = request.options.metric;
        Self::start(
            command,
            host,
            Operation::Scan(request.clone()),
            archive,
            move |input, progress| {
                let mut snapshot =
                    read_snapshot_with_progress(input, progress)?;
                if metric != crate::tree::Metric::Bytes {
                    aggregate(&mut snapshot.tree, metric);
                }
                Ok(snapshot)
            },
        )
    }
}

impl RemoteRoots {
    pub fn discover(host: &str) -> io::Result<Self> {
        let (command, archive) = ssh_command(host)?;
        Self::start(
            command,
            host,
            Operation::Discover {
                discover_version: VERSION,
            },
            archive,
            |input, _| read_roots(input),
        )
    }
}

impl<T: Send + 'static> RemoteJob<T> {
    fn start(
        mut command: Command,
        host: &str,
        operation: Operation,
        archive: Option<crate::payload::Archive>,
        decode: impl FnOnce(
            &mut BufReader<std::process::ChildStdout>,
            &Shared,
        ) -> io::Result<T>
        + Send
        + 'static,
    ) -> io::Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stderr = child.stderr.take().expect("piped stderr");
        // Drain concurrently so noisy login scripts cannot block discovery.
        let diagnostics = std::thread::spawn(move || read_diagnostics(stderr));
        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let child = Arc::new(Mutex::new(child));
        let worker = child.clone();
        let host = host.to_owned();
        let (tx, result) = mpsc::channel();
        let progress = Shared::default();
        let live = progress.clone();
        std::thread::spawn(move || {
            let mut input = BufReader::new(stdout);
            let response = (|| {
                if let Some(archive) = archive {
                    crate::deployment::deploy_with_progress(
                        &archive,
                        &mut input,
                        &mut stdin,
                        |value| stage(&live, value),
                    )?;
                }
                stage(&live, Stage::Starting);
                send(
                    &mut stdin,
                    &SessionRequest {
                        operation,
                        watch_disconnect: true,
                        live_progress: true,
                        client_dedup: true,
                        live_tree: true,
                    },
                )?;
                decode(&mut input, &live)
            })();
            drop(stdin);
            if response.is_err() {
                let _ = worker.lock().expect("SSH process lock").kill();
            }
            // Never hold the process lock across wait(): cancellation must
            // be able to acquire it even after a response was read.
            let status = loop {
                let status =
                    worker.lock().expect("SSH process lock").try_wait();
                match status {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) => {}
                    Err(error) => break Err(error),
                }
                std::thread::park_timeout(std::time::Duration::from_millis(10));
            };
            let details = diagnostics.join().unwrap_or_default();
            let outcome = match (response, status) {
                (Ok(response), Ok(status)) if status.success() => Ok(response),
                (Err(error), _) | (_, Err(error)) => {
                    Err(ssh_error(&host, &details, &error))
                }
                (_, Ok(status)) => Err(ssh_error(
                    &host,
                    &details,
                    &io::Error::other(format!("SSH exited with {status}")),
                )),
            };
            let _ = tx.send(outcome);
        });
        Ok(Self {
            child,
            result,
            progress,
        })
    }

    pub fn progress(&self) -> Progress {
        self.progress.lock().expect("remote progress lock").clone()
    }

    pub fn poll(&self) -> Option<io::Result<T>> {
        self.result.try_recv().ok()
    }
}

impl<T> Drop for RemoteJob<T> {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Request {
        Request {
            root: None,
            disk: false,
            options: ScanOptions::default(),
        }
    }

    #[cfg(unix)]
    fn wait<T: Send + 'static>(scan: &RemoteJob<T>) -> io::Result<T> {
        wait_for(scan, std::time::Duration::from_secs(10))
    }

    fn wait_for<T: Send + 'static>(
        scan: &RemoteJob<T>,
        timeout: std::time::Duration,
    ) -> io::Result<T> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(result) = scan.poll() {
                return result;
            }
            assert!(std::time::Instant::now() < deadline, "SSH scan hung");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[cfg(unix)]
    fn failing_process(script: &str) -> io::Error {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        let handle =
            RemoteScan::spawn_command(command, "dev", &request()).unwrap();
        wait(&handle).unwrap_err()
    }

    #[cfg(unix)]
    #[test]
    fn ssh_authentication_diagnostics_reach_the_caller() {
        let error = failing_process(
            "read -r request; echo 'Permission denied (publickey,password).' \
             >&2; exit 255",
        )
        .to_string();
        assert!(error.contains("Permission denied"), "{error}");
        assert!(error.contains("verify your credentials"), "{error}");
        assert!(!error.contains("truncated"), "{error}");
        assert!(!error.contains("Install disktree-scan"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn missing_helper_diagnostics_name_the_server() {
        let error = failing_process(
            "read -r request; echo 'disktree-scan: command not found' \
             >&2; exit 127",
        )
        .to_string();
        assert!(error.contains("command not found"), "{error}");
        assert!(error.contains("ON dev, not on this computer"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn banner_and_truncated_output_are_reported_without_a_hang() {
        for output in ["hello", "hello\\n"] {
            let error =
                failing_process(&format!("read -r request; printf '{output}'"))
                    .to_string();
            assert!(error.contains("SSH dev:"), "{error}");
        }
    }

    // The helper runs on this machine, which must be a Unix host.
    #[cfg(unix)]
    #[test]
    fn discovery_is_not_a_legacy_scan_request() {
        let session = SessionRequest {
            operation: Operation::Discover {
                discover_version: VERSION,
            },
            watch_disconnect: true,
            live_progress: true,
            client_dedup: true,
            live_tree: true,
        };
        let bytes = serde_json::to_vec(&session).unwrap();
        assert!(serde_json::from_slice::<Request>(&bytes).is_err());
        let mut input = bytes;
        input.push(b'\n');
        let mut output = Vec::new();
        serve(&mut input.as_slice(), &mut output).unwrap();
        let roots = read_roots(&mut output.as_slice()).unwrap();
        assert!(roots.volumes.iter().all(|volume| volume.space.is_none()));
        assert!(!roots.platform.is_empty());
        assert!(roots.identity.is_some());
    }

    // The helper runs on this machine, which must be a Unix host.
    #[cfg(unix)]
    #[test]
    fn discovery_never_resolves_a_scan_path_and_rejects_unknown_versions() {
        let mut output = Vec::new();
        // No scan fields: an old helper fails to decode this, rather than
        // falling back to its default home-directory walk.
        serve(&mut b"{\"discover_version\":1,\"root\":\"/does-not-exist-disktree-test\"}\n".as_slice(),
            &mut output).unwrap();
        assert!(read_roots(&mut output.as_slice()).is_ok());
        assert!(
            serve(
                &mut b"{\"discover_version\":99}\n".as_slice(),
                &mut Vec::new()
            )
            .is_err()
        );
    }

    #[test]
    fn invalid_discovery_records_are_not_accepted_as_roots() {
        for value in [
            serde_json::json!({"version":99,"home":null,"platform":"linux","identity":null,"volumes":[]}),
            serde_json::json!({"version":1,"home":null,"platform":"linux","identity":null,"volumes":[{"point":"relative","device":null,"space":null}]}),
        ] {
            let mut data = serde_json::to_vec(&value).unwrap();
            data.push(b'\n');
            assert!(read_roots(&mut data.as_slice()).is_err());
        }
    }

    #[test]
    fn diagnostics_are_bounded_and_controls_are_stripped() {
        let mut data = vec![b'x'; MAX_DIAGNOSTICS * 4];
        data.extend_from_slice(b"\x1b[31mPermission denied\r\n");
        let text = read_diagnostics(data.as_slice());
        assert!(text.len() <= MAX_DIAGNOSTICS);
        assert!(text.ends_with("Permission denied"));
        assert!(!text.contains(['\x1b', '\r']));
    }

    // Uses an explicitly supplied config, never the user's default SSH host.
    #[cfg(unix)]
    #[test]
    #[ignore = "requires DISKTREE_TEST_SSH_CONFIG with isolated sshd"]
    fn real_ssh_transport_returns_a_snapshot() {
        let config = std::env::var("DISKTREE_TEST_SSH_CONFIG").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep"), b"hello").unwrap();
        let home = tempfile::tempdir().unwrap();
        let bin = home.path().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let helper = std::env::var("DISKTREE_TEST_HELPER").unwrap();
        std::os::unix::fs::symlink(helper, bin.join("disktree-scan")).unwrap();
        // Force the documented ~/.local/bin fallback, not the ambient PATH.
        let remote = format!(
            "HOME={} PATH=/nonexistent; export HOME PATH;\n{REMOTE_COMMAND}",
            crate::export::shell_quote(home.path().to_str().unwrap()),
        );
        let mut command = Command::new("ssh");
        command.args(["-F", &config, "-T", "-C", "fixture", &remote]);
        let request = Request {
            root: Some(dir.path().into()),
            options: ScanOptions {
                apparent_size: true,
                ..ScanOptions::default()
            },
            ..request()
        };
        let handle =
            RemoteScan::spawn_command(command, "fixture", &request).unwrap();
        let snapshot = wait(&handle).unwrap();
        assert_eq!(snapshot.tree.bytes, 5);
        assert_eq!(snapshot.tree.files, 1);
        assert_eq!(std::fs::read(dir.path().join("keep")).unwrap(), b"hello");
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires DISKTREE_TEST_SSH_CONFIG with isolated sshd"]
    fn real_ssh_transport_reports_auth_and_install_errors() {
        let config = std::env::var("DISKTREE_TEST_SSH_CONFIG").unwrap();
        let mut denied = Command::new("ssh");
        denied.args([
            "-F",
            &config,
            "-T",
            "-o",
            "PubkeyAuthentication=no",
            "-o",
            "PreferredAuthentications=none",
            "fixture",
            REMOTE_COMMAND,
        ]);
        let scan =
            RemoteScan::spawn_command(denied, "fixture", &request()).unwrap();
        let error = wait(&scan).unwrap_err().to_string();
        assert!(error.contains("Permission denied"), "{error}");

        let home = tempfile::tempdir().unwrap();
        let remote = format!(
            "HOME={} PATH=/nonexistent; export HOME PATH;\n{REMOTE_COMMAND}",
            crate::export::shell_quote(home.path().to_str().unwrap()),
        );
        let mut missing = Command::new("ssh");
        missing.args(["-F", &config, "-T", "fixture", &remote]);
        let scan =
            RemoteScan::spawn_command(missing, "fixture", &request()).unwrap();
        let error = wait(&scan).unwrap_err().to_string();
        assert!(error.contains("not installed on this server"), "{error}");
        assert!(error.contains("ON fixture"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires isolated sshd and DISKTREE_TEST_ARCHIVE"]
    fn real_ssh_transport_bootstraps_and_checks_cached_payloads() {
        let config = std::env::var("DISKTREE_TEST_SSH_CONFIG").unwrap();
        let archive = crate::payload::Archive::open(
            PathBuf::from(std::env::var("DISKTREE_TEST_ARCHIVE").unwrap())
                .as_path(),
        )
        .unwrap()
        .unwrap();
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("keep"), b"hello").unwrap();
        // Exercise the shipped static helper beyond the removed 256-level
        // cutoff, through deployment and decoding, not just the walker.
        let mut deep = data.path().to_path_buf();
        for _ in 0..320 {
            deep.push("d");
            std::fs::create_dir(&deep).unwrap();
        }
        std::fs::write(deep.join("leaf"), b"world").unwrap();
        let cache = home.path().join("cache");
        let remote = format!(
            "HOME={} XDG_CACHE_HOME={}; export HOME XDG_CACHE_HOME;\n{}",
            crate::export::shell_quote(home.path().to_str().unwrap()),
            crate::export::shell_quote(cache.to_str().unwrap()),
            crate::deployment::COMMAND,
        );
        let arch = std::env::consts::ARCH;
        let (entry, bytes) = archive.load(arch).unwrap();
        let cached = cache
            .join("disktree/ssh")
            .join(&entry.sha256)
            .join("disktree-scan");
        let request = Request {
            root: Some(data.path().into()),
            options: ScanOptions {
                apparent_size: true,
                ..ScanOptions::default()
            },
            ..request()
        };
        for attempt in 0..3 {
            let mut command = Command::new("ssh");
            command.args(["-F", &config, "-T", "fixture", &remote]);
            let handle = RemoteScan::spawn_with_archive(
                command,
                "fixture",
                &request,
                Some(archive.clone()),
            )
            .unwrap();
            let snapshot = wait(&handle).unwrap();
            assert_eq!(snapshot.tree.bytes, 10);
            assert_eq!(snapshot.tree.files, 2);
            assert_eq!(handle.progress().files, 2);
            assert!(snapshot.header.identity.is_some());
            assert_eq!(std::fs::read(&cached).unwrap(), bytes);
            if attempt == 1 {
                // A damaged cache must be repaired, never executed.
                let damaged = cached.with_extension("damaged");
                std::fs::write(&damaged, b"broken").unwrap();
                std::fs::rename(damaged, &cached).unwrap();
            }
        }
        assert!(!home.path().join(".local/bin/disktree-scan").exists());
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "requires isolated sshd and DISKTREE_TEST_ARCHIVE"]
    fn real_ssh_transport_discovers_roots_without_a_scan() {
        let config = std::env::var("DISKTREE_TEST_SSH_CONFIG").unwrap();
        let archive = crate::payload::Archive::open(
            PathBuf::from(std::env::var("DISKTREE_TEST_ARCHIVE").unwrap())
                .as_path(),
        )
        .unwrap()
        .unwrap();
        let home = tempfile::tempdir().unwrap();
        let cache = home.path().join("cache");
        let remote = format!(
            "HOME={} XDG_CACHE_HOME={}; export HOME XDG_CACHE_HOME;\n{}",
            crate::export::shell_quote(home.path().to_str().unwrap()),
            crate::export::shell_quote(cache.to_str().unwrap()),
            crate::deployment::COMMAND,
        );
        let mut command = Command::new("ssh");
        command.args(["-F", &config, "-T", "fixture", &remote]);
        let job = RemoteRoots::start(
            command,
            "fixture",
            Operation::Discover {
                discover_version: VERSION,
            },
            Some(archive),
            |input, _| read_roots(input),
        )
        .unwrap();
        let roots = wait(&job).unwrap();
        assert_eq!(roots.home.as_deref(), Some(home.path()));
        assert!(roots.identity.is_some());
        assert!(
            roots
                .volumes
                .iter()
                .all(|volume| volume.point.is_absolute()
                    && volume.space.is_none())
        );
        // Only the checked cache was installed, not a per-root scan log,
        // helper in PATH, or a traversed home tree.
        assert!(!home.path().join(".local").exists());
    }

    /// Explicit opt-in only: never contact a real machine in the test gate.
    #[test]
    #[ignore = "requires DISKTREE_TEST_SSH_HOST and DISKTREE_TEST_SSH_ROOT"]
    fn live_ssh_scan_uses_the_production_transport() {
        let host = std::env::var("DISKTREE_TEST_SSH_HOST").unwrap();
        let root = std::env::var("DISKTREE_TEST_SSH_ROOT").unwrap();
        let scan = RemoteScan::spawn(
            &host,
            &Request {
                root: Some(PathBuf::from(&root)),
                ..request()
            },
        )
        .unwrap();
        let snapshot =
            wait_for(&scan, std::time::Duration::from_mins(3)).unwrap();
        assert!(snapshot.tree.is_dir());
        assert!(snapshot.header.root.is_absolute());
        println!(
            "SSH {}: {} — {} bytes, {} files, {} directories; space: {:?}",
            host,
            snapshot.header.root.display(),
            snapshot.tree.bytes,
            snapshot.tree.files,
            snapshot.tree.dirs,
            snapshot.header.space,
        );
    }

    fn unix_stream(platform: &str) -> Vec<u8> {
        let mut data = Vec::new();
        let header = Header {
            version: VERSION,
            root: PathBuf::from("/srv"),
            home: Some(PathBuf::from("/home/someone")),
            disk_root: Some(PathBuf::from("/")),
            space: None,
            platform: platform.into(),
            identity: None,
        };
        send(&mut data, &header).unwrap();
        for name in [r"a\b", "c:d"] {
            send(&mut data, &Record(name.into(), 1, 5, 1, 0, false, 0))
                .unwrap();
        }
        send(&mut data, &Record("root".into(), 0, 0, 0, 0, false, 2)).unwrap();
        send(&mut data, &"done").unwrap();
        data
    }

    /// Every client reads a Unix host's names as the host wrote them: `\`
    /// and `:` are ordinary characters there, and the joined path uses `/`.
    #[test]
    fn unix_names_with_backslashes_and_colons_reach_every_client() {
        let snapshot = read_snapshot(&mut unix_stream("linux").as_slice())
            .expect("a Unix host's names are accepted");
        assert_eq!(snapshot.tree.files, 2);
        let style = PathStyle::Posix;
        for name in [r"a\b", "c:d"] {
            assert!(snapshot.tree.child_named(name).is_some());
            let path = style.join(&snapshot.header.root, name);
            assert_eq!(path.to_str().unwrap(), format!("/srv/{name}"));
        }
    }

    /// Remote paths are joined as POSIX paths, so a Windows host is refused
    /// by name rather than half-read with the wrong separator.
    #[test]
    fn windows_hosts_and_non_posix_roots_are_refused() {
        let error = read_snapshot(&mut unix_stream("windows").as_slice())
            .unwrap_err()
            .to_string();
        assert!(error.contains("Windows hosts"), "{error}");
        for value in [
            serde_json::json!({"version":1,"home":null,"platform":"windows","identity":null,"volumes":[]}),
            serde_json::json!({"version":1,"home":"C:\\Users\\x","platform":"linux","identity":null,"volumes":[]}),
            serde_json::json!({"version":1,"home":null,"platform":"linux","identity":null,"volumes":[{"point":"C:\\","device":null,"space":null}]}),
        ] {
            let mut data = serde_json::to_vec(&value).unwrap();
            data.push(b'\n');
            assert!(read_roots(&mut data.as_slice()).is_err(), "{value}");
        }
    }

    #[test]
    fn rejects_ssh_options_and_shell_syntax() {
        for host in ["-oProxyCommand=oops", "dev; touch x", "dev\nother", ""] {
            assert!(validate_host(host).is_err());
        }
        for host in ["dev", "user@dev", "[::1]"] {
            assert!(validate_host(host).is_ok());
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_backslashes_in_names_roundtrip_without_becoming_separators() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(r"back\slash"), b"x").unwrap();
        let request = Request {
            root: Some(dir.path().into()),
            disk: false,
            options: ScanOptions {
                apparent_size: true,
                ..ScanOptions::default()
            },
        };
        let mut input = Vec::new();
        send(&mut input, &request).unwrap();
        let mut data = Vec::new();
        serve(&mut input.as_slice(), &mut data).unwrap();
        let snapshot = read_snapshot(&mut data.as_slice()).unwrap();
        assert_eq!(snapshot.tree.bytes, 1);
        assert!(snapshot.tree.child_named(r"back\slash").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn live_counters_are_available_before_the_tree_finishes() {
        use std::os::unix::net::UnixStream;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), b"hello").unwrap();
        let request = Request {
            root: Some(dir.path().into()),
            disk: false,
            options: ScanOptions {
                apparent_size: true,
                ..ScanOptions::default()
            },
        };
        let mut data = Vec::new();
        serve_request(request, &mut data).unwrap();
        let mut lines = data.split_inclusive(|byte| *byte == b'\n');
        let header = lines.next().unwrap();
        let leaf = lines.next().unwrap();
        let tail = lines.flatten().copied().collect::<Vec<_>>();
        let (mut output, input) = UnixStream::pair().unwrap();
        input
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let shared = Shared::default();
        let live = shared.clone();
        let worker = std::thread::spawn(move || {
            read_snapshot_with_progress(&mut BufReader::new(input), &live)
        });
        output.write_all(header).unwrap();
        send(&mut output, &serde_json::json!({"enter": ""})).unwrap();
        output.write_all(leaf).unwrap();
        send(&mut output, &serde_json::json!({"directory": dir.path()}))
            .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let progress = shared.lock().unwrap().clone();
            if progress.files == 1 {
                assert_eq!(progress.bytes, 5);
                assert_eq!(progress.stage, Stage::Indexing);
                let preview =
                    progress.preview.as_ref().expect("preview before done");
                assert_eq!(preview.entries[0].bytes, 5);
                assert_eq!(preview.entries[0].name, "Files in root");
                assert!(!worker.is_finished());
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        output.write_all(&tail).unwrap();
        let snapshot = worker.join().unwrap().unwrap();
        assert_eq!(snapshot.tree.bytes, 5);
        assert_eq!(shared.lock().unwrap().stage, Stage::Finalizing);
    }

    // The live preview this checks comes from the Unix helper's streaming
    // walk; on other systems `serve` sends a finished tree with no preview.
    #[cfg(unix)]
    #[test]
    fn desktop_dedup_counts_links_once_without_requiring_external_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("first"), b"hello").unwrap();
        std::fs::hard_link(root.join("first"), root.join("alias")).unwrap();
        std::fs::hard_link(root.join("first"), dir.path().join("outside"))
            .unwrap();
        std::fs::create_dir_all(root.join("nested/deeper")).unwrap();
        std::fs::write(root.join("nested/deeper/file"), b"world").unwrap();
        for dedup in [true, false] {
            let session = SessionRequest {
                operation: Operation::Scan(Request {
                    root: Some(root.clone()),
                    disk: false,
                    options: ScanOptions {
                        apparent_size: true,
                        dedup_hardlinks: dedup,
                        ..ScanOptions::default()
                    },
                }),
                live_progress: true,
                client_dedup: true,
                live_tree: true,
                watch_disconnect: false,
            };
            let mut data = Vec::new();
            serve_session(session, &mut data).unwrap();
            let shared = Shared::default();
            let snapshot =
                read_snapshot_with_progress(&mut data.as_slice(), &shared)
                    .unwrap();
            assert_eq!(snapshot.tree.bytes, if dedup { 10 } else { 15 });
            assert_eq!(snapshot.tree.files, 3);
            let preview = shared.lock().unwrap().preview.clone().unwrap();
            let nested = preview
                .entries
                .iter()
                .find(|entry| entry.name == "nested")
                .unwrap();
            assert_eq!(nested.bytes, 5);
            assert_eq!(nested.files, 1);
            assert_eq!(
                preview.entries.iter().map(|e| e.bytes).sum::<u64>(),
                snapshot.tree.bytes
            );
        }
    }

    // The helper runs on this machine, which must be a Unix host.
    #[cfg(unix)]
    #[test]
    fn desktop_dedup_accepts_more_than_the_old_pending_link_limit() {
        let mut data = Vec::new();
        send(
            &mut data,
            &Header {
                version: VERSION,
                root: std::env::temp_dir().join("links-test"),
                home: None,
                disk_root: None,
                space: None,
                platform: std::env::consts::OS.into(),
                identity: None,
            },
        )
        .unwrap();
        let count = 65_537_u64;
        for index in 0..=count {
            let inode = if index == count { 42 } else { index };
            send(
                &mut data,
                &serde_json::json!({
                    "link": [7, inode],
                    "entry": [format!("file-{index}"), 1, 1, 1, 0, false, 0],
                }),
            )
            .unwrap();
        }
        send(
            &mut data,
            &Record(
                "root".into(),
                0,
                0,
                0,
                0,
                false,
                usize::try_from(count + 1).unwrap(),
            ),
        )
        .unwrap();
        send(&mut data, &"done").unwrap();
        let snapshot = read_snapshot(&mut data.as_slice()).unwrap();
        assert_eq!(snapshot.tree.files, count + 1);
        assert_eq!(snapshot.tree.bytes, count);
    }

    // The helper runs on this machine, which must be a Unix host.
    #[cfg(unix)]
    #[test]
    fn protocol_roundtrip_and_truncation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a ' file"), b"hello").unwrap();
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        let request = Request {
            root: Some(dir.path().into()),
            disk: false,
            options: ScanOptions {
                apparent_size: true,
                ..ScanOptions::default()
            },
        };
        let mut data = Vec::new();
        let mut input = Vec::new();
        send(&mut input, &request).unwrap();
        serve(&mut input.as_slice(), &mut data).unwrap();
        assert!(!data.windows(11).any(|part| part == b"\"directory\""));
        let snapshot = read_snapshot(&mut data.as_slice()).unwrap();
        assert_eq!(snapshot.tree.bytes, 5);
        assert_eq!(snapshot.tree.files, 1);
        assert!(snapshot.tree.child_named("empty").unwrap().is_dir());
        data.truncate(data.len() - 7);
        assert!(read_snapshot(&mut data.as_slice()).is_err());
        assert!(dir.path().join("a ' file").exists());
    }
}

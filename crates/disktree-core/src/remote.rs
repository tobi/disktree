//! Scanning another machine over SSH.
//!
//! The other machine does the walk: `find` lists everything under a path
//! with its type, blocks, length and last write, and the tree is built here
//! from that listing. Nothing is installed there, and nothing here reaches
//! past the user's own `ssh`: its keys, its agent, its `known_hosts` and its
//! config are what decide whether the connection is made.
//!
//! A tree built this way can be looked at and nothing else. Its paths are
//! the other machine's, so nothing that acts on a local path — removal
//! above all — may be offered for it; see the application's state.
//!
//! The listing needs GNU `find` for `-printf`, which is every Linux and not
//! macOS or the BSDs.

use std::io::{self, BufRead, BufReader, Read as _};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::scan::{ScanOptions, ScanProgress};
use crate::tree::{Node, NodeKind};

/// Somewhere to scan on another machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Remote {
    /// Who and where, as `ssh` takes it: `user@host`, or a `Host` from the
    /// user's SSH config.
    pub target: String,
    /// The port, when the address named one.
    pub port: Option<u16>,
    /// The directory there, absolute.
    pub path: String,
}

impl Remote {
    /// Read `user@host:/path`, as `scp` writes it, or
    /// `ssh://user@host[:port]/path`. `None` for anything else, a local
    /// path above all: `C:/x` has a one-letter "host", which no address
    /// written this way may have.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (target, port, path) =
            if let Some(rest) = text.strip_prefix("ssh://") {
                let (authority, path) = rest.split_once('/')?;
                let (target, port) = match authority.rsplit_once(':') {
                    Some((target, port)) => (target, Some(port.parse().ok()?)),
                    None => (authority, None),
                };
                (target, port, format!("/{path}"))
            } else {
                let (target, path) = text.split_once(':')?;
                if target.chars().count() < 2 {
                    return None;
                }
                (target, None, path.to_owned())
            };
        // A target that starts with a dash would be read by `ssh` as an
        // option, and one of those runs a command of its choosing.
        let plain = !target.is_empty()
            && !target.starts_with('-')
            && target.chars().all(|char| {
                char.is_ascii_alphanumeric() || "@._-".contains(char)
            });
        // `http://x/y` splits the same way, into a path of two slashes.
        let rooted = path.starts_with('/') && !path.starts_with("//");
        (plain && rooted && !path.contains('\0')).then(|| Self {
            target: target.to_owned(),
            port,
            path: tidy(&path),
        })
    }

    /// The same place with another directory.
    #[must_use]
    pub fn at(&self, path: &str) -> Self {
        Self {
            path: tidy(path),
            ..self.clone()
        }
    }
}

impl std::fmt::Display for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.target, self.path)
    }
}

/// `path` with forward slashes and no trailing one, `/` itself aside. A
/// path that came back from a Windows `PathBuf` has both kinds.
fn tidy(path: &str) -> String {
    let path = path.replace('\\', "/");
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// `target` with its host spelled as the user's SSH config spells it.
///
/// `ssh` matches a `Host` line by case, so `mybox` does not find the
/// entry for `MyBox` and is looked up as a name on the network instead,
/// which fails with nothing to say why. A name is not told apart by case
/// anywhere else it is typed, so the entry's own spelling is used when
/// the typed one names no entry exactly. Patterns are left to `ssh`.
fn as_configured(target: &str, config: &str) -> String {
    let (user, host) = match target.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, target),
    };
    let names: Vec<&str> = config
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (word, rest) = line
                .split_once(|char: char| char.is_whitespace() || char == '=')?;
            word.eq_ignore_ascii_case("host").then_some(rest)
        })
        .flat_map(|names| names.split([' ', '\t', '=']))
        .filter(|name| !name.is_empty() && !name.contains(['*', '?', '!']))
        .collect();
    let spelled = if names.contains(&host) {
        host
    } else {
        names
            .into_iter()
            .find(|name| name.eq_ignore_ascii_case(host))
            .unwrap_or(host)
    };
    match user {
        Some(user) => format!("{user}@{spelled}"),
        None => spelled.to_owned(),
    }
}

/// `text` as one word of a POSIX shell: the path is the one thing in the
/// remote command that is not ours, and it must stay a path.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The command the other machine runs. One record per entry, NUL-ended
/// since a name may hold a newline: type, 512-byte blocks, length, last
/// write, and the path below the start.
fn listing_command(path: &str, options: &ScanOptions) -> String {
    // The slash after the path has `find` enter a start that is a link
    // to a directory, as `/bin` and many a `/var/www` are; without it
    // the link alone is listed and there is no tree.
    format!(
        "LC_ALL=C find {}/ {}-printf '%y\\t%b\\t%s\\t%T@\\t%P\\0'",
        quoted(path),
        if options.one_filesystem { "-xdev " } else { "" },
    )
}

/// List `remote` over SSH and build its tree, sizes not yet totalled.
/// Blocks until the listing ends or `progress` is cancelled.
pub(crate) fn scan(
    remote: &Remote,
    options: &ScanOptions,
    progress: &ScanProgress,
) -> io::Result<Node> {
    let mut command = Command::new("ssh");
    command
        // No prompt can be answered from a window: a key, an agent or the
        // config has to be enough, and a host not yet known is refused
        // rather than trusted on first sight.
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
    if let Some(port) = remote.port {
        command.args(["-p", &port.to_string()]);
    }
    let config = std::env::home_dir()
        .and_then(|home| {
            std::fs::read_to_string(home.join(".ssh").join("config")).ok()
        })
        .unwrap_or_default();
    command
        .arg("--")
        .arg(as_configured(&remote.target, &config))
        .arg(listing_command(&remote.path, options))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // `CREATE_NO_WINDOW`: no console flashes up beside the window.
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().map_err(|error| {
        io::Error::new(error.kind(), format!("cannot start ssh: {error}"))
    })?;
    let stdout = child.stdout.take().ok_or_else(|| io::Error::other("ssh"))?;
    let mut stderr =
        child.stderr.take().ok_or_else(|| io::Error::other("ssh"))?;
    // Read beside the listing: `find` complains about every directory it
    // may not enter, and a full pipe would stop it.
    let complaints = std::thread::spawn(move || {
        let mut said = String::new();
        let _ = stderr.read_to_string(&mut said);
        said
    });
    let name = remote.path.rsplit('/').next().unwrap_or_default();
    let name = if name.is_empty() { "/" } else { name };
    let tree = read_listing(BufReader::new(stdout), name, options, progress);
    if progress.is_cancelled() {
        let _ = child.kill();
    }
    let status = child.wait()?;
    let said = complaints.join().unwrap_or_default();
    for line in said.lines().filter(|line| line.starts_with("find:")) {
        progress
            .record_error(Path::new(&remote.target), &io::Error::other(line));
    }
    let tree = tree?;
    if let Some(error) = cut_short(status.code(), &said) {
        return Err(error);
    }
    match tree {
        Some(tree) => Ok(tree),
        // Nothing listed at all: `ssh` or `find` said why.
        None => Err(io::Error::other(
            said.lines()
                .find(|line| !line.trim().is_empty())
                .map_or_else(
                    || format!("ssh ended with {status} and listed nothing"),
                    str::to_owned,
                ),
        )),
    }
}

/// Why a listing that ended is not the whole of it, when it is not. `ssh`
/// ends with 255 for a failure of its own — a connection refused, or one
/// that dropped half way — and with the remote command's status
/// otherwise; `find` ends with 1 for a directory it may not enter, which
/// is an ordinary scan. Half a tree must not pass for a whole one: its
/// totals would be believed.
fn cut_short(status: Option<i32>, said: &str) -> Option<io::Error> {
    (status == Some(255)).then(|| {
        io::Error::other(
            said.lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with("find:"))
                .map_or_else(
                    || "the ssh connection ended before the listing did".into(),
                    str::to_owned,
                ),
        )
    })
}

/// Build the tree from a listing. `find` names a directory before what is
/// in it, so the directories still open are a stack: a record at a depth
/// closes every one at that depth or below, each into its parent.
/// `None` when the listing has no record for the start itself.
fn read_listing(
    mut listing: impl BufRead,
    root_name: &str,
    options: &ScanOptions,
    progress: &ScanProgress,
) -> io::Result<Option<Node>> {
    let mut open: Vec<Node> = Vec::new();
    // Depth below which entries are skipped, inside a hidden directory.
    let mut skipping: Option<usize> = None;
    let mut record = Vec::new();
    loop {
        record.clear();
        if listing.read_until(0, &mut record)? == 0 {
            break;
        }
        if progress.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "cancelled",
            ));
        }
        if record.last() == Some(&0) {
            record.pop();
        }
        let Some(entry) = Entry::parse(&record) else {
            continue;
        };
        let depth = if entry.path.is_empty() {
            0
        } else {
            entry.path.split('/').count()
        };
        if skipping.is_some_and(|hidden| depth > hidden) {
            continue;
        }
        skipping = None;
        let name = entry.path.rsplit('/').next().unwrap_or_default();
        if depth > 0 && !options.include_hidden && name.starts_with('.') {
            skipping = Some(depth);
            continue;
        }
        // A record can only be placed in a directory that is open; one
        // that arrives early is out of order and has nowhere to go.
        if depth > open.len() {
            continue;
        }
        close(&mut open, depth);
        let bytes = if options.apparent_size {
            entry.length
        } else {
            entry.blocks.saturating_mul(512)
        };
        if entry.kind == NodeKind::Directory {
            let mut node =
                Node::directory(if depth == 0 { root_name } else { name });
            node.modified = entry.modified;
            open.push(node);
            progress.add(0, 1, 0);
        } else if let Some(parent) = open.last_mut() {
            // Sockets, fifos and devices are addressable, not space.
            let bytes = if entry.kind == NodeKind::Other {
                0
            } else {
                bytes
            };
            let mut node = Node::entry(name, entry.kind, bytes);
            node.modified = entry.modified;
            parent.children.push(node);
            progress.add(u64::from(entry.kind == NodeKind::File), 0, bytes);
        }
    }
    close(&mut open, 1);
    Ok(open.pop())
}

/// Close every open directory at `depth` or below into its parent.
fn close(open: &mut Vec<Node>, depth: usize) {
    while open.len() > depth.max(1) {
        if let Some(done) = open.pop()
            && let Some(parent) = open.last_mut()
        {
            parent.children.push(done);
        }
    }
}

/// One record of the listing.
struct Entry {
    kind: NodeKind,
    blocks: u64,
    length: u64,
    modified: i64,
    /// Below the start; empty for the start itself.
    path: String,
}

impl Entry {
    fn parse(record: &[u8]) -> Option<Self> {
        let mut fields = record.splitn(5, |byte| *byte == b'\t');
        let kind = match fields.next()? {
            b"d" => NodeKind::Directory,
            b"f" => NodeKind::File,
            b"l" => NodeKind::Symlink,
            _ => NodeKind::Other,
        };
        let number = |field: Option<&[u8]>| {
            std::str::from_utf8(field?).ok()?.parse::<u64>().ok()
        };
        let blocks = number(fields.next())?;
        let length = number(fields.next())?;
        // Seconds with a fraction; the whole seconds are what is kept.
        let modified = std::str::from_utf8(fields.next()?)
            .ok()?
            .split('.')
            .next()?
            .parse()
            .ok()?;
        let path = String::from_utf8_lossy(fields.next()?).into_owned();
        Some(Self {
            kind,
            blocks,
            length,
            modified,
            path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::{Metric, aggregate};

    #[test]
    fn a_host_is_found_in_the_config_whatever_its_case() {
        let config = "# mine\nHost box-HS other\n    HostName 10.0.0.9\n\
                      Host *.lan !gate\n  User me\nhost=Lower\n";
        assert_eq!(as_configured("Box-hs", config), "box-HS");
        assert_eq!(as_configured("me@BOX-HS", config), "me@box-HS");
        assert_eq!(as_configured("OTHER", config), "other");
        assert_eq!(as_configured("lower", config), "Lower");
        // Spelled as an entry is, or naming none: as typed.
        assert_eq!(as_configured("box-HS", config), "box-HS");
        assert_eq!(as_configured("elsewhere", config), "elsewhere");
        // A pattern is ssh's to match, and `HostName` is not `Host`.
        assert_eq!(as_configured("X.LAN", config), "X.LAN");
        assert_eq!(as_configured("10.0.0.9", config), "10.0.0.9");
        assert_eq!(as_configured("me@box", ""), "me@box");
    }

    #[test]
    fn a_listing_ssh_did_not_finish_is_not_a_tree() {
        // `find` could not enter a directory: an ordinary scan.
        assert!(
            cut_short(Some(1), "find: '/root': Permission denied").is_none()
        );
        assert!(cut_short(Some(0), "").is_none());
        // `ssh` itself failed, before the listing or part way through it.
        let dropped = cut_short(
            Some(255),
            "find: '/root': Permission denied\nConnection to host closed.\n",
        )
        .expect("an error");
        assert_eq!(dropped.to_string(), "Connection to host closed.");
        assert!(cut_short(Some(255), "").is_some());
    }

    fn listing(records: &[&str]) -> Vec<u8> {
        records
            .iter()
            .flat_map(|record| record.bytes().chain([0]))
            .collect()
    }

    fn read(records: &[&str], options: &ScanOptions) -> Option<Node> {
        let progress = ScanProgress::default();
        let mut tree = read_listing(
            listing(records).as_slice(),
            "data",
            options,
            &progress,
        )
        .expect("a listing")?;
        aggregate(&mut tree, Metric::Bytes);
        Some(tree)
    }

    const RECORDS: [&str; 8] = [
        "d\t8\t4096\t1700000000.5\t",
        "d\t8\t4096\t1700000001.0\tlogs",
        "f\t16\t5000\t1700000002.0\tlogs/app.log",
        "d\t8\t4096\t1700000003.0\tlogs/old",
        "f\t8\t100\t1700000004.0\tlogs/old/a\nb.log",
        "d\t8\t4096\t1700000005.0\t.cache",
        "f\t2048\t1000000\t1700000006.0\t.cache/blob",
        "l\t0\t11\t1700000007.0\tlink",
    ];

    #[test]
    fn an_address_is_read_as_scp_or_as_a_url_and_a_local_path_is_not() {
        let remote = Remote::parse(" me@box.lan:/var/log/ ").expect("scp");
        assert_eq!(remote.target, "me@box.lan");
        assert_eq!(remote.path, "/var/log");
        assert_eq!(remote.port, None);
        assert_eq!(remote.to_string(), "me@box.lan:/var/log");
        let remote = Remote::parse("ssh://me@box:2222/srv").expect("url");
        assert_eq!((remote.port, remote.path.as_str()), (Some(2222), "/srv"));
        assert_eq!(Remote::parse("box:/").expect("root").path, "/");
        assert_eq!(remote.at("\\srv\\www\\").path, "/srv/www");
        for local in [
            "C:/Users",
            "C:\\Users",
            "/home/me",
            "box:relative",
            "-oProxyCommand=x:/tmp",
            "me@box;rm:/tmp",
            "ssh://box",
            "http://box/path",
        ] {
            assert_eq!(Remote::parse(local), None, "{local}");
        }
    }

    #[test]
    fn the_path_stays_one_word_whatever_is_in_it() {
        let command =
            listing_command("/srv/it's; rm -rf ~", &ScanOptions::default());
        assert!(
            command.contains("find '/srv/it'\\''s; rm -rf ~'/ -xdev -printf"),
            "{command}"
        );
    }

    #[test]
    fn a_listing_becomes_the_tree_it_lists() {
        let tree = read(&RECORDS, &ScanOptions::default()).expect("a tree");
        assert_eq!(tree.name.as_ref(), "data");
        // Blocks, not lengths: 16 + 8 + 2048 of them.
        assert_eq!(tree.bytes, (16 + 8 + 2048) * 512);
        assert_eq!(tree.files, 3);
        let logs = tree.child_named("logs").expect("logs");
        assert_eq!(logs.bytes, 24 * 512);
        // A name with a newline in it is one entry, in the right place.
        let old = logs.child_named("old").expect("old");
        assert!(old.child_named("a\nb.log").is_some());
        assert_eq!(logs.modified, 1_700_000_004);
        assert!(tree.child_named("link").is_some());
    }

    #[test]
    fn lengths_and_hidden_entries_follow_the_scan_options() {
        let options = ScanOptions {
            apparent_size: true,
            include_hidden: false,
            ..ScanOptions::default()
        };
        let tree = read(&RECORDS, &options).expect("a tree");
        // Without `.cache` and what is in it; the link's length counts.
        assert!(tree.child_named(".cache").is_none());
        assert_eq!(tree.bytes, 5000 + 100 + 11);
    }

    #[test]
    fn nothing_listed_is_no_tree() {
        assert!(read(&[], &ScanOptions::default()).is_none());
        // A record that is not one is passed over.
        assert!(read(&["find: nonsense"], &ScanOptions::default()).is_none());
    }
}

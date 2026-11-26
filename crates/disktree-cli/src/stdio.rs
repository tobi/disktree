//! Read-only `--stdio-v1` SSH remote protocol server.
//!
//! Invoked over SSH by `disktree` (auto-deployed from the portable binary or
//! found on the server's `PATH`). Uses `disktree-scanner`'s local `redb` SWR
//! cache on the remote host — the exact same database `disktree-cli` uses when
//! run locally — so warm scans return in milliseconds, stale scans stream
//! cached data first while revalidating in parallel, and cold scans walk in
//! parallel and populate the remote host's cache. Enforces a 1 GiB
//! address-space ceiling on Linux, a 30-minute wall deadline, and exits
//! immediately if the SSH pipe closes so a blocked filesystem never outlives
//! the connection.

use std::io::{self, BufReader, BufWriter, Read as _, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use disktree_core::remote::{
    Operation, Request, SessionRequest, write_arena, write_header,
    write_marker, write_progress,
};
use disktree_scanner::{
    CachePolicy, Freshness, HostKey, ScanEvent, Scanner, StorageMode,
};

const STDOUT_BUFFER_BYTES: usize = 256 * 1024;
const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(200);

pub fn serve_stdio_v1() -> io::Result<()> {
    limit_resources()?;
    // A live client is not permission to monopolise the server forever,
    // including when a filesystem syscall cannot be cancelled.
    std::thread::Builder::new()
        .name("scan-deadline".into())
        .spawn(|| {
            std::thread::park_timeout(Duration::from_mins(30));
            eprintln!("Remote scan exceeded 30 minutes; scan a smaller path.");
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
                        Err(error)
                            if error.kind() == io::ErrorKind::Interrupted => {}
                        Ok(0) => {
                            // Exit the process, not just a scan task: a
                            // blocked filesystem must not outlive SSH.
                            std::process::exit(0);
                        }
                        _ => {
                            eprintln!(
                                "Remote scan connection closed or sent unexpected input."
                            );
                            std::process::exit(1);
                        }
                    }
                }
            })?;
    }
    let mut out =
        BufWriter::with_capacity(STDOUT_BUFFER_BYTES, io::stdout().lock());
    serve_with_remote_db(session, &mut out)
}

fn serve_with_remote_db(
    session: SessionRequest,
    out: &mut impl Write,
) -> io::Result<()> {
    let SessionRequest {
        operation,
        watch_disconnect,
        live_progress,
        client_dedup,
        live_tree,
        stream_stale,
        force_refresh,
    } = session;
    let request = match operation {
        Operation::Discover { .. } => {
            return disktree_core::remote::serve_session(
                SessionRequest {
                    operation,
                    watch_disconnect,
                    live_progress,
                    client_dedup,
                    live_tree,
                    stream_stale,
                    force_refresh,
                },
                out,
            );
        }
        Operation::Scan(request) => request,
    };

    if request.options.follow_links || !request.options.one_filesystem {
        return Err(io::Error::other(
            "Remote scans never follow symlinks or cross mounts; \
             select that mount as a separate scan root.",
        ));
    }

    let home = std::env::home_dir();
    let root = request
        .root
        .clone()
        .or_else(|| home.clone())
        .ok_or_else(|| io::Error::other("no remote home directory"))?;
    let root = if request.disk {
        disktree_core::space::volume_root_for(&root)
            .unwrap_or_else(|| PathBuf::from("/"))
    } else {
        root
    }
    .canonicalize()?;
    if !root.is_dir() {
        return Err(io::Error::other("scan root is not a directory"));
    }

    // Use the remote host's own redb SWR cache DB when available. If storage
    // cannot be opened at all, fall back to the bounded streaming walker.
    let Ok(scanner) = Scanner::open_default().or_else(|_| {
        Scanner::new(StorageMode::InMemory, CachePolicy::default())
    }) else {
        return disktree_core::remote::serve_session(
            SessionRequest {
                operation: Operation::Scan(request),
                watch_disconnect,
                live_progress,
                client_dedup,
                live_tree,
                stream_stale,
                force_refresh,
            },
            out,
        );
    };

    write_header(&root, home, out)?;
    serve_scan_from_scanner(
        &scanner,
        root,
        &request,
        StreamFlags {
            live_progress,
            client_dedup,
            stream_stale,
            force_refresh,
        },
        out,
    )
}

#[derive(Clone, Copy)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "protocol opt-in flags mirrored from SessionRequest"
)]
struct StreamFlags {
    live_progress: bool,
    client_dedup: bool,
    stream_stale: bool,
    force_refresh: bool,
}

fn serve_scan_from_scanner<W: Write>(
    scanner: &Scanner,
    root: PathBuf,
    request: &Request,
    flags: StreamFlags,
    out: &mut W,
) -> io::Result<()> {
    let policy = CachePolicy {
        force_refresh: flags.force_refresh,
        stale_while_revalidate: true,
        ..CachePolicy::default()
    };
    let session = scanner.open_with(
        HostKey::Local,
        root,
        request.options.to_scanner(),
        policy,
    )?;

    let mut last_heartbeat = Instant::now();
    let mut streamed_final = false;
    let mut streamed_stale = false;
    let apparent = request.options.apparent_size;

    let emit_progress = |last: &mut Instant, out: &mut W| -> io::Result<()> {
        if !flags.live_progress || last.elapsed() < HEARTBEAT_INTERVAL {
            return Ok(());
        }
        let p = session.progress();
        let stats = p.snapshot(0);
        let dir = p
            .active_path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let bytes = if apparent {
            stats.apparent_bytes
        } else {
            stats.allocated_bytes
        };
        write_progress(
            dir.as_deref(),
            stats.files,
            stats.dirs,
            bytes,
            stats.errors,
            out,
        )?;
        *last = Instant::now();
        Ok(())
    };

    loop {
        match session.recv_timeout(HEARTBEAT_INTERVAL) {
            Ok(ScanEvent::Snapshot(snap)) => match snap.freshness {
                Freshness::Partial { .. } => {}
                Freshness::Fresh { .. } => {
                    write_arena(
                        &snap.arena,
                        &request.options,
                        flags.client_dedup,
                        out,
                    )?;
                    streamed_final = true;
                }
                Freshness::Stale { .. } => {
                    if flags.stream_stale && !streamed_stale {
                        write_arena(
                            &snap.arena,
                            &request.options,
                            flags.client_dedup,
                            out,
                        )?;
                        write_marker("stale", out)?;
                        streamed_stale = true;
                        last_heartbeat = Instant::now();
                    }
                }
                Freshness::Live { .. } => {
                    write_arena(
                        &snap.arena,
                        &request.options,
                        flags.client_dedup,
                        out,
                    )?;
                    out.flush()?;
                    streamed_final = true;
                }
            },
            Ok(ScanEvent::Finished) => {
                // Waiting until `Finished` guarantees the remote host's cache
                // DB commit completed before `"done"` lets the client close
                // stdin and exit this helper.
                if !streamed_final {
                    return Err(io::Error::other(
                        "remote scan finished without producing a snapshot",
                    ));
                }
                write_marker("done", out)?;
                return Ok(());
            }
            Ok(ScanEvent::Error { message }) => {
                return Err(io::Error::other(message));
            }
            Ok(
                ScanEvent::SubtreeUpdating { .. }
                | ScanEvent::SubtreeRefreshed { .. }
                | ScanEvent::Progress { .. }
                | ScanEvent::StorageFallback { .. },
            )
            | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                emit_progress(&mut last_heartbeat, out)?;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if streamed_final {
                    write_marker("done", out)?;
                    return Ok(());
                }
                return Err(io::Error::other(
                    "remote scanner disconnected unexpectedly",
                ));
            }
        }
    }
}

/// Kernel-enforced, not a sampled RSS check that can miss an allocation
/// burst. Scales with host RAM (`total / 4`, clamped to 4..=32 GiB) so large
/// multi-million-file dev boxes never abort at 1 GiB while runaway allocations
/// still cannot exhaust the host.
#[cfg(target_os = "linux")]
fn limit_resources() -> io::Result<()> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    const MIN_ADDRESS_SPACE: u64 = 4 << 30;
    const DEFAULT_ADDRESS_SPACE: u64 = 8 << 30;
    const MAX_ADDRESS_SPACE: u64 = 32 << 30;

    let cap = linux_total_ram_bytes().map_or(DEFAULT_ADDRESS_SPACE, |total| {
        (total / 4).clamp(MIN_ADDRESS_SPACE, MAX_ADDRESS_SPACE)
    });
    let inherited = getrlimit(Resource::As);
    let maximum = inherited.maximum.unwrap_or(u64::MAX).min(cap);
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

#[cfg(target_os = "linux")]
fn linux_total_ram_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kib.checked_mul(1024)
}

#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps, reason = "same fallible API as Linux")]
const fn limit_resources() -> io::Result<()> {
    Ok(())
}

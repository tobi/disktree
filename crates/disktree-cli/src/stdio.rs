//! Read-only `--stdio-v1` SSH remote protocol server.
//!
//! Invoked over SSH by `disktree` (auto-deployed from the portable binary or
//! found on the server's `PATH`). Enforces a 1 GiB address-space ceiling on
//! Linux, a 30-minute wall deadline, and exits immediately if the SSH pipe
//! closes so a blocked filesystem never outlives the connection.

use std::io::{self, BufReader, BufWriter, Read as _};
use std::time::Duration;

const STDOUT_BUFFER_BYTES: usize = 256 * 1024;

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
    disktree_core::remote::serve_session(
        session,
        &mut BufWriter::with_capacity(STDOUT_BUFFER_BYTES, io::stdout().lock()),
    )
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

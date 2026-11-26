//! One replaceable progress snapshot, never a queue of filesystem events.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stage {
    #[default]
    Connecting,
    Authenticating,
    CheckingHelper,
    UploadingHelper,
    Starting,
    Indexing,
    Finalizing,
}

impl Stage {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Connecting => "Connecting over SSH",
            Self::Authenticating => "Waiting for Tailscale SSH approval",
            Self::CheckingHelper => "Checking remote scanner",
            Self::UploadingHelper => "Uploading remote scanner",
            Self::Starting => "Starting remote scanner",
            Self::Indexing => "Indexing remotely",
            Self::Finalizing => "Building the treemap",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub stage: Stage,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: u64,
    pub directory: Option<PathBuf>,
    pub auth_url: Option<String>,
    pub updated: Option<Instant>,
    pub preview: Option<Arc<crate::scan_preview::Preview>>,
    /// Stale or incremental partial snapshot streamed from the remote host
    /// while parallel scanning/revalidation runs on the remote host.
    pub stale_snapshot: Option<Arc<crate::remote::Snapshot>>,
    /// Monotonic counter incremented each time a new `stale_snapshot` arrives.
    pub stale_seq: u64,
}

/// Extract a Tailscale SSH check/login URL from SSH stderr diagnostics.
///
/// Strictly bounded to `https://login.tailscale.com/` and safe URL characters
/// so untrusted remote stderr can never direct the browser elsewhere.
#[must_use]
pub fn extract_tailscale_auth_url(text: &str) -> Option<String> {
    const PREFIX: &str = "https://login.tailscale.com/";
    let start = text.find(PREFIX)?;
    let tail = &text[start..];
    let len = tail
        .bytes()
        .take_while(|b| {
            b.is_ascii_alphanumeric() || b"-._~:/?#[]@!$&'()*+,;=%".contains(b)
        })
        .count();
    let url = tail[..len].trim_end_matches(['.', ',', ')', ']']);
    (url.len() > PREFIX.len()).then(|| url.to_owned())
}

pub(crate) fn auth_required(shared: &Shared, url: String) {
    let mut progress = shared.lock().expect("remote progress lock");
    if progress.auth_url.as_deref() != Some(url.as_str()) {
        progress.auth_url = Some(url);
        progress.stage = Stage::Authenticating;
        progress.updated = Some(Instant::now());
    }
}

/// Cap both wire heartbeats and UI publications at five per second.
pub(crate) const INTERVAL: Duration = Duration::from_millis(200);
pub(crate) type Shared = Arc<Mutex<Progress>>;

pub(crate) fn stage(shared: &Shared, value: Stage) {
    let mut progress = shared.lock().expect("remote progress lock");
    progress.stage = value;
    if value != Stage::Authenticating {
        progress.auth_url = None;
    }
    progress.updated = Some(Instant::now());
}

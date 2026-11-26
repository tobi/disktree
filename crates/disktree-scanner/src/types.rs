//! Core configuration, status, freshness, and identity types for
//! `disktree-scanner`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Default cache freshness window: 15 minutes.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_mins(15);

/// Minimum available bytes required on the cache volume before opening or
/// growing the persistent cache database.
pub const DEFAULT_MIN_FREE_HEADROOM: u64 = 64 * 1024 * 1024;

/// Default maximum cache database size before LRU eviction kicks in (256 MiB).
pub const DEFAULT_MAX_DB_BYTES: u64 = 256 * 1024 * 1024;

/// What a node represents on disk.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    #[default]
    Directory,
    File,
    Symlink,
    /// Sockets, fifos, and device nodes: addressable, zero space.
    Other,
}

impl NodeKind {
    pub const fn is_dir(self) -> bool {
        matches!(self, Self::Directory)
    }

    pub const fn to_bits(self) -> u32 {
        match self {
            Self::Directory => 0,
            Self::File => 1,
            Self::Symlink => 2,
            Self::Other => 3,
        }
    }

    pub const fn from_bits(bits: u32) -> Self {
        match bits & 0b11 {
            0 => Self::Directory,
            1 => Self::File,
            2 => Self::Symlink,
            _ => Self::Other,
        }
    }
}

/// How children are ranked or weighted in queries and views.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// Allocated disk space (`st_blocks * 512` / filesystem allocation).
    #[default]
    Bytes,
    /// Logical file length (`st_size`).
    ApparentBytes,
    /// Total file count at or beneath the node.
    Files,
}

/// Scheduling priority for revalidation work.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Ongoing SWR refresh of the broader tree.
    #[default]
    Background,
    /// User-focused folder (clicked tile / explicit `r` refresh): preempts
    /// new background admissions until complete, then resumes background work.
    Immediate,
}

/// Per-directory freshness & active update status exposed to UIs and CLI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SubtreeStatus {
    /// Within TTL or just revalidated.
    Fresh { scanned_at: u64 },
    /// Served from cache older than TTL; queued for revalidation.
    Stale { scanned_at: u64 },
    /// Actively being scanned by workers right now.
    Updating { previous_scanned_at: Option<u64> },
    /// Ancestor stub or unvisited directory.
    Unscanned,
}

impl SubtreeStatus {
    pub const fn tag(self) -> u32 {
        match self {
            Self::Fresh { .. } => 0,
            Self::Stale { .. } => 1,
            Self::Updating { .. } => 2,
            Self::Unscanned => 3,
        }
    }

    pub const fn from_tag(tag: u32, scanned_at: u64) -> Self {
        match tag & 0b11 {
            0 => Self::Fresh { scanned_at },
            1 => Self::Stale { scanned_at },
            2 => Self::Updating {
                previous_scanned_at: if scanned_at == 0 {
                    None
                } else {
                    Some(scanned_at)
                },
            },
            _ => Self::Unscanned,
        }
    }
}

/// Whole-snapshot freshness summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Freshness {
    /// Entire requested subtree came from fresh cache (<= TTL).
    Fresh { oldest_at: u64, newest_at: u64 },
    /// Entire requested subtree came from cache, with some/all dirs older
    /// than TTL.
    Stale {
        oldest_at: u64,
        newest_at: u64,
        stale_dirs: u32,
    },
    /// Only part of the requested subtree was cached (e.g. widened from a
    /// previously scanned child).
    Partial {
        cached_dirs: u32,
        missing_dirs: u32,
        oldest_at: Option<u64>,
        newest_at: Option<u64>,
    },
    /// Freshly completed live scan (all requested subtrees up to date).
    Live { scanned_at: u64 },
}

/// How the scanner stores or streams scan data.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum StorageMode {
    /// Use the default persistent cache DB; automatically fall back to
    /// `InMemory` if the cache volume is unwritable, read-only, or low on
    /// free space.
    #[default]
    Auto,
    /// Use an explicit cache DB path; falls back to `InMemory` on I/O errors
    /// unless `strict` is true.
    Persistent { path: PathBuf, strict: bool },
    /// Create a temporary cache DB in the OS temp directory (cleaned up on
    /// drop); falls back to `InMemory` on I/O errors.
    TempDb,
    /// Pure in-memory compact arena; never touches disk for caching.
    InMemory,
    /// Bounded-memory O(depth) streaming mode; never retains a full tree or
    /// writes a cache DB.
    Streaming,
}

/// The effective storage mode in use after fallback resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveStorageMode {
    Persistent,
    TempDb,
    InMemory,
    Streaming,
}

/// Why the scanner automatically downgraded from a disk-backed cache DB.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum FallbackReason {
    /// Cache volume had less than the required free-space headroom.
    LowDiskSpace { available: u64, required: u64 },
    /// Kernel returned `ENOSPC` / `StorageFull`.
    DiskFull,
    /// Cache directory lives on a read-only filesystem (`EROFS`).
    ReadOnlyFilesystem,
    /// Permission denied opening or creating the cache directory/file.
    PermissionDenied,
    /// Database backend unavailable, corrupt, or compiled without `cache-db`.
    DatabaseUnavailable { detail: String },
}

/// Freshness and revalidation policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachePolicy {
    /// Maximum age before a cached directory is considered stale (default 15m).
    pub ttl: Duration,
    /// Return stale/partial cached snapshots immediately while revalidating
    /// in the background.
    pub stale_while_revalidate: bool,
    /// Optional upper bound past which cached entries are ignored even for SWR.
    pub max_stale: Option<Duration>,
    /// During a walk, skip nested subtrees that are still fresh (<= TTL).
    pub reuse_fresh_subtrees: bool,
    /// Ignore fresh cache hits and force a live walk of the requested root
    /// (still persists fresh results if storage is writable).
    pub force_refresh: bool,
    /// Minimum free bytes required on the cache volume before opening/writing
    /// the cache DB (default 64 MiB).
    pub min_free_headroom: u64,
    /// Soft cap on cache DB size before LRU eviction (default 256 MiB).
    pub max_db_bytes: u64,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            ttl: DEFAULT_CACHE_TTL,
            stale_while_revalidate: true,
            max_stale: None,
            reuse_fresh_subtrees: true,
            force_refresh: false,
            min_free_headroom: DEFAULT_MIN_FREE_HEADROOM,
            max_db_bytes: DEFAULT_MAX_DB_BYTES,
        }
    }
}

/// How a scan traverses and measures the filesystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanOptions {
    /// Rank/report apparent size (`st_size`) instead of allocated blocks.
    /// Both metrics are recorded during every walk so toggling this never
    /// invalidates cached entries.
    pub apparent_size: bool,
    /// Follow symlinks. Off by default.
    pub follow_links: bool,
    /// Include dotfiles and hidden entries in returned views. Walks record
    /// hidden entries with a flag so toggling this does not invalidate cache.
    pub include_hidden: bool,
    /// Stop at mount boundaries (including bind mounts and subvolumes).
    pub one_filesystem: bool,
    /// Optional maximum directory depth below the scan root.
    pub max_depth: Option<usize>,
    /// Deduplicate hardlinked files (`nlink > 1`) so bytes are counted once.
    pub dedup_hardlinks: bool,
    /// Optional explicit worker count; `None` uses available parallelism.
    pub threads: Option<usize>,
    /// Directory-enumeration / metadata strategy (`Auto` by default).
    #[serde(default)]
    pub backend: crate::backend::ScanBackend,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            apparent_size: false,
            follow_links: false,
            include_hidden: true,
            one_filesystem: true,
            max_depth: None,
            dedup_hardlinks: true,
            threads: None,
            backend: crate::backend::ScanBackend::Auto,
        }
    }
}

impl ScanOptions {
    /// Bitmask of traversal-shaping flags that affect what is stored in cache.
    ///
    /// `apparent_size`, `include_hidden`, and `dedup_hardlinks` are excluded
    /// because both byte metrics, the hidden flag, and multi-link identities
    /// are always stored and applied at query/aggregation time.
    pub const fn cache_topology_mask(&self) -> u32 {
        let mut mask = 0u32;
        if self.follow_links {
            mask |= 1 << 0;
        }
        if self.one_filesystem {
            mask |= 1 << 1;
        }
        mask
    }

    /// Preferred metric for sorting/display given `apparent_size`.
    pub const fn byte_metric(&self) -> Metric {
        if self.apparent_size {
            Metric::ApparentBytes
        } else {
            Metric::Bytes
        }
    }
}

/// Machine identity used to namespace cache entries so hosts are never mixed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HostIdentity {
    pub hostname: String,
    pub machine_id: Option<String>,
}

impl HostIdentity {
    /// Capture the local machine's identity, if readable.
    pub fn current() -> Option<Self> {
        let hostname = if cfg!(target_os = "linux") {
            std::fs::read_to_string("/proc/sys/kernel/hostname").ok()?
        } else {
            String::from_utf8(
                std::process::Command::new("hostname").output().ok()?.stdout,
            )
            .ok()?
        };
        let hostname = hostname.trim().to_owned();
        if hostname.is_empty()
            || !hostname
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c))
        {
            return None;
        }
        let machine_id = std::fs::read_to_string("/etc/machine-id")
            .ok()
            .map(|id| id.trim().to_owned())
            .filter(|id| {
                id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit())
            });
        Some(Self {
            hostname,
            machine_id,
        })
    }

    /// Canonical cache namespace key derived from host identity, never an
    /// SSH alias.
    pub fn cache_key(&self) -> String {
        match &self.machine_id {
            Some(id) => format!("remote:{}:{id}", self.hostname),
            None => format!("remote:{}", self.hostname),
        }
    }
}

/// Host namespace in the cache DB.
#[derive(
    Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize,
)]
pub enum HostKey {
    #[default]
    Local,
    Remote(HostIdentity),
}

impl HostKey {
    pub fn as_db_key(&self) -> String {
        match self {
            Self::Local => "local".to_owned(),
            Self::Remote(identity) => identity.cache_key(),
        }
    }
}

/// Capacity of the volume a path lives on.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize,
)]
pub struct SpaceInfo {
    pub total: u64,
    pub free: u64,
    pub available: u64,
}

impl SpaceInfo {
    pub const fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    pub fn used_fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.used() as f64 / self.total as f64) as f32
        }
    }

    /// Free space after `bytes` are removed, saturating at the volume size and
    /// never counting the same byte twice.
    #[must_use]
    pub fn after_removing(&self, bytes: u64) -> Self {
        let available = self.available.saturating_add(bytes).min(self.total);
        let free = self.free.saturating_add(bytes).min(self.total);
        Self {
            total: self.total,
            free,
            available,
        }
    }
}

/// Summary counters for a scan or cache query.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanStats {
    pub files: u64,
    pub dirs: u64,
    pub allocated_bytes: u64,
    pub apparent_bytes: u64,
    pub errors: u64,
    /// Number of directories reused directly from cache without walking disk.
    pub skipped_fresh_dirs: u64,
    /// Number of directories walked on disk.
    pub walked_dirs: u64,
    pub duration_ms: u64,
}

/// Diagnostic stats for the cache database.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStats {
    pub db_path: Option<PathBuf>,
    pub db_bytes: u64,
    pub hosts: u64,
    pub indexed_dirs: u64,
    pub chunks: u64,
}

/// Current Unix timestamp in seconds.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

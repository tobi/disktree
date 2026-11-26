//! `disktree-scanner`: high-efficiency filesystem scanner with a compact
//! arena tree, prefix-trie front-coded string pool, columnar Protobuf cache
//! DB, priority-interruptible stale-while-revalidate (SWR), and automatic
//! fallback to in-memory or streaming modes.

pub mod backend;
pub mod db;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(windows)]
pub mod mft;
pub mod mounts;
pub mod proto;
pub mod session;
pub mod stream;
pub mod strings;
pub mod tree;
pub mod types;
pub mod walk;
#[cfg(windows)]
pub mod windows;

pub use backend::{DirEntryMeta, ScanBackend};
pub use db::{CacheDb, CachedSubtree, InvalidateReport, SubtreeCatalog};
pub use mounts::{default_cache_db_path, space_info};
pub use session::{ScanEvent, ScanSession, ScanSnapshot, Scanner};
pub use stream::{
    StreamConfig, StreamItem, stream_walk, stream_walk_with_config,
};
pub use strings::{NameId, StringPool};
pub use tree::{ArenaNode, HardlinkEntry, NodeFlags, NodeId, TreeArena};
pub use types::{
    ActiveStorageMode, CachePolicy, CacheStats, DEFAULT_CACHE_TTL,
    DEFAULT_MAX_DB_BYTES, DEFAULT_MIN_FREE_HEADROOM, FallbackReason, Freshness,
    HostIdentity, HostKey, Metric, NodeKind, Priority, ScanOptions, ScanStats,
    SpaceInfo, StorageMode, SubtreeStatus, unix_now,
};
pub use walk::{
    DirFreshness, DirTallyHook, PauseGate, SkipOracle, WalkConfig, WalkOutcome,
    WalkProgress,
};

# Changelog

All notable changes to disktree are documented here.

## [1.0.0] - 2026-11-25

### Highlights
- **Standalone `disktree-scanner` engine:** compact 64-byte arena nodes, prefix-trie front-coded string pool, columnar Protobuf + LZ4 persistent cache DB (`redb`, 15-minute default TTL), priority-interruptible stale-while-revalidate (SWR), and automatic fallback to in-memory/streaming when the cache disk is full, read-only, or unwritable.
- **Unified `disktree-cli` binary:** Ratatui interactive TUI, structured `--json` / `--ndjson` / `--stream` modes, `cache` & `invalidate` subcommands, and the `--stdio-v1` read-only SSH remote server (replacing the separate `disktree-scan` crate).
- **Read-only SSH exploration with zero-setup Linux deployment:** discover remote mounts without scanning, stream bounded-memory walks over SSH, auto-deploy embedded static `x86_64` / `aarch64` `disktree-cli` helpers from portable Linux builds, or offer one-click remote installation (and copyable `curl` one-liner) from predictable GitHub release URLs.
- **Live freshness & queue visibility:** unscanned (no-data) directories walk ahead of stale ones (oldest first), clicking into a stale directory preempts background work to refresh it immediately, and treemap tiles shade **Stale** (washed + muted corner pip), **Queued** (lightly veiled + dashed accent border), and **Refreshing** (accent tint + solid accent border) distinctly.

### Added
- **Desktop UX & Omarchy integration:**
  - Direct `$HOME` scan on startup with live treemap previews during cold scans, default treemap depth `4`, and a tabbed **Open location** dialog (`This computer` / `SSH remote`) with native **Browse…** folder picking and `~` expansion.
  - Compact, static-height **Selection** card with top-right **Open**, **Mark/Unmark**, and **Refresh** icon buttons so **Worth a look** and **Marked** never jump while hovering or marking tiles.
  - Instant zero-rescan toggles for **Hidden files** (`i`) and **Apparent size** (`d`) re-projected directly from `TreeArena`.
  - Follow the Omarchy desktop font live across every surface, including `gpui-omarchy` controls (#126).
  - Omarchy-styled app icon (`assets/disktree.svg` and `.ico`) (#128).
  - Close the `?` key list via a Close button or clicking outside (#89).
- **Linux Btrfs awareness:** local `/` scans include distinct same-filesystem Btrfs subvolumes (such as Omarchy's `/home`) while keeping duplicate bind mounts, snapshots, other disks, and network mounts out.
- **Packaging & releases:**
  - Predictable raw release binaries (`disktree-cli-{linux,macos,windows}-{x86_64,aarch64}`) uploaded alongside desktop archives; Linux tarballs and `make install` include both `disktree` and `disktree-cli`.
  - Windows Inno Setup installer built and checked in CI (#108).

### Performance & Architecture
- Automatic worker-count heuristic inside `disktree-scanner` (scales smoothly up to 8 workers without UI configuration; `--scan-threads N` stays as an override).
- In-place Size/Files re-ranking (`Arc::make_mut`), lazy formatting for only the 150 drawn treemap labels, `select_nth_unstable_by` layout selection, cached marked crumbs, and off-UI-thread `space_info` polling (#84).
- Explicit `State` / `Event` / `Effect` / `step` state machines across find/filter, location discovery, scan lifecycle, worker pause gate, priority walk phases, storage fallback, and CLI TUI modes.
- Emit live snapshots to interactive UIs immediately before committing Protobuf shards to the cache DB, and never display incomplete partial ancestor stubs as a complete tree.

### Security & Fixes
- Refuse removal on paths containing lossy UTF-8 replacement characters (`U+FFFD`) (#82).
- Harden Git checkout inspection against repo-controlled external diff, `textconv`, inherited `GIT_CONFIG_*`, and transport protocols (#81, #84).
- Treat SSH remote paths as POSIX paths on every client, including Windows (#114, #115).
- Exclude Linux kernel/virtual pseudo-filesystems (`/proc/*`, `/sys/*`, `/dev/*`, `binfmt_misc`, `bpf`, `efivarfs`, `ramfs`, `gvfsd-fuse`) from volume candidates (#106).
- Sync test fixture allocation writes on CoW/ZFS filesystems (#109) and pin GitHub Actions workflows to commit SHAs with per-step signing secrets (#112, #84).
- Fix `Node::find` BFS ordering, shift-scroll pan clamping, and saturating byte totals (#84).

## [0.11.0] - 2026-10-04
- macOS `getattrlistbulk` bulk directory listing and compact 88-byte `Node` layout (#53).
- Cross-volume free-space projection attribution (#56).
- Right-click reveal in file manager (#49), non-Unicode export flagging (#52), and size rounding fixes (#70).
- Snapshot-booted Btrfs root subvolume handling.

## [0.10.1] - 2026-09-25
- Windows support, NTFS MFT fast scan, and macOS bundle signing/notarization releases.

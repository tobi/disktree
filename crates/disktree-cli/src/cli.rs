//! Command-line argument parsing and human duration parsing for
//! `disktree-cli`.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use disktree_scanner::{CachePolicy, Metric, ScanOptions, StorageMode};

/// Fast disk usage explorer with a 15-minute Protobuf cache, SWR, and JSON.
#[derive(Clone, Debug, Parser)]
#[command(name = "disktree-cli", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Directory to inspect (defaults to current working directory).
    pub path: Option<PathBuf>,

    /// Maximum cache age before revalidation (default: 15m). Examples: 0, 30s,
    /// 15m, 1h, 2h30m.
    #[arg(long, default_value = "15m", value_parser = parse_duration)]
    pub max_age: Duration,

    /// Ignore cached freshness and force a live scan (still updates cache).
    #[arg(long, alias = "no-cache")]
    pub fresh: bool,

    /// Return stale cached data immediately without waiting for revalidation
    /// (in `--json` / non-TTY mode).
    #[arg(long)]
    pub stale_ok: bool,

    /// Force revalidation of specific subtrees before/during the scan.
    #[arg(long = "revalidate", value_name = "PATH")]
    pub revalidate_paths: Vec<PathBuf>,

    /// Storage backend mode (auto falls back to in-memory on full/RO disks).
    #[arg(long, value_enum, default_value_t = ModeArg::Auto)]
    pub mode: ModeArg,

    /// Override the cache DB directory (stores `scan-cache.redb` inside it).
    #[arg(long, value_name = "DIR")]
    pub cache_dir: Option<PathBuf>,

    /// Output a single structured JSON document (`schema_version: 1`).
    #[arg(long)]
    pub json: bool,

    /// Output newline-delimited JSON (streaming records or SWR snapshots).
    #[arg(long)]
    pub ndjson: bool,

    /// Run in bounded-memory O(depth) streaming mode (implies `--ndjson`).
    #[arg(long)]
    pub stream: bool,

    /// With `--ndjson`, emit stale cached snapshot first then live snapshot.
    #[arg(long)]
    pub swr: bool,

    /// Maximum depth emitted in JSON/text output (unlimited if omitted).
    #[arg(long)]
    pub depth: Option<usize>,

    /// Maximum number of children emitted per directory in JSON/text output.
    #[arg(long)]
    pub top: Option<usize>,

    /// Measure and rank by apparent file length instead of allocated blocks.
    #[arg(short = 'a', long)]
    pub apparent_size: bool,

    /// Exclude hidden dotfiles/dot-directories from totals and display.
    #[arg(short = 'H', long)]
    pub no_hidden: bool,

    /// Follow symbolic links while walking.
    #[arg(short = 'l', long)]
    pub follow_links: bool,

    /// Cross filesystem mount boundaries while walking.
    #[arg(short = 'X', long)]
    pub cross_filesystems: bool,

    /// Ranking metric for children.
    #[arg(long, value_enum, default_value_t = MetricArg::Bytes)]
    pub metric: MetricArg,

    /// Fixed worker threads for scanning (defaults to CPU count).
    #[arg(long)]
    pub scan_threads: Option<usize>,
}

#[derive(Clone, Debug, Subcommand)]
pub enum Command {
    /// Mark one or more paths dirty in the cache DB (e.g. after deleting files)
    /// so subsequent runs revalidate them immediately.
    Invalidate {
        /// Paths to invalidate (may already be deleted).
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Emit JSON output.
        #[arg(long)]
        json: bool,
    },
    /// Inspect or maintain the scan cache database.
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
}

#[derive(Clone, Debug, Subcommand)]
pub enum CacheAction {
    /// Print cache DB location, size, indexed directories, and chunk count.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Delete all cached entries.
    Clear {
        #[arg(long)]
        json: bool,
    },
    /// Remove cached chunks older than `--older-than` (default: 24h).
    Prune {
        #[arg(long, default_value = "24h", value_parser = parse_duration)]
        older_than: Duration,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ModeArg {
    Auto,
    Memory,
    Stream,
    TempDb,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum MetricArg {
    Bytes,
    Apparent,
    Files,
}

impl Cli {
    pub fn storage_mode(&self) -> StorageMode {
        if self.stream || self.mode == ModeArg::Stream {
            return StorageMode::Streaming;
        }
        if let Some(dir) = &self.cache_dir {
            return StorageMode::Persistent {
                path: dir.join("scan-cache.redb"),
                strict: false,
            };
        }
        match self.mode {
            ModeArg::Auto => StorageMode::Auto,
            ModeArg::Memory => StorageMode::InMemory,
            ModeArg::Stream => StorageMode::Streaming,
            ModeArg::TempDb => StorageMode::TempDb,
        }
    }

    pub fn scan_options(&self) -> ScanOptions {
        ScanOptions {
            apparent_size: self.apparent_size
                || self.metric == MetricArg::Apparent,
            follow_links: self.follow_links,
            include_hidden: !self.no_hidden,
            one_filesystem: !self.cross_filesystems,
            max_depth: None,
            dedup_hardlinks: true,
            threads: self.scan_threads,
        }
    }

    pub const fn metric(&self) -> Metric {
        if self.apparent_size {
            return Metric::ApparentBytes;
        }
        match self.metric {
            MetricArg::Bytes => Metric::Bytes,
            MetricArg::Apparent => Metric::ApparentBytes,
            MetricArg::Files => Metric::Files,
        }
    }

    pub fn cache_policy(&self) -> CachePolicy {
        CachePolicy {
            ttl: self.max_age,
            stale_while_revalidate: true,
            force_refresh: self.fresh,
            reuse_fresh_subtrees: !self.fresh,
            ..CachePolicy::default()
        }
    }
}

/// Parse a compact human duration such as `0`, `45s`, `15m`, `1h`, `1h30m`,
/// or `2d`.
pub fn parse_duration(input: &str) -> Result<Duration, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("duration cannot be empty".to_owned());
    }
    if trimmed == "0" {
        return Ok(Duration::ZERO);
    }
    if let Ok(plain_secs) = trimmed.parse::<u64>() {
        return Ok(Duration::from_secs(plain_secs));
    }

    let mut total_secs = 0u64;
    let mut num_buf = String::new();
    let mut matched_unit = false;

    for ch in trimmed.chars() {
        if ch.is_ascii_digit() {
            num_buf.push(ch);
            continue;
        }
        if num_buf.is_empty() {
            return Err(format!(
                "invalid duration '{input}': unexpected '{ch}'"
            ));
        }
        let val: u64 = num_buf.parse().map_err(|e| {
            format!("invalid number in duration '{input}': {e}")
        })?;
        num_buf.clear();

        let mult = match ch {
            's' | 'S' => 1u64,
            'm' | 'M' => 60,
            'h' | 'H' => 3_600,
            'd' | 'D' => 86_400,
            _ => {
                return Err(format!(
                    "unknown unit '{ch}' in duration '{input}' (use s, m, h, d)"
                ));
            }
        };
        total_secs = total_secs.saturating_add(val.saturating_mul(mult));
        matched_unit = true;
    }

    if !num_buf.is_empty() || !matched_unit {
        return Err(format!(
            "invalid duration '{input}' (example: 15m, 1h30m)"
        ));
    }

    Ok(Duration::from_secs(total_secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_duration_handles_units_and_compounds() {
        assert_eq!(parse_duration("0").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("15m").unwrap(), Duration::from_mins(15));
        assert_eq!(parse_duration("1h30m").unwrap(), Duration::from_mins(90));
        assert_eq!(parse_duration("2d").unwrap(), Duration::from_hours(48));
        assert!(parse_duration("abc").is_err());
    }
}

//! `disktree-cli`: fast terminal disk usage explorer (Ratatui TUI + JSON mode)
//! backed by `disktree-scanner`.

mod cli;
mod format;
mod json_out;
mod tui;

use std::io::{self, IsTerminal as _, Write as _};
use std::path::PathBuf;

use anyhow::Result;
use clap::Parser as _;
use disktree_scanner::{
    Freshness, HostKey, ScanEvent, Scanner, StorageMode, space_info,
};
use serde::Serialize;

use crate::cli::{CacheAction, Cli, Command};
use crate::format::{human_age, human_bytes};
use crate::json_out::{build_document, write_ndjson_line, write_pretty};

fn main() -> Result<()> {
    let args = Cli::parse();
    let policy = args.cache_policy();
    let mode = args.storage_mode();

    if let Some(command) = &args.command {
        return run_subcommand(command, mode, policy);
    }

    let root = match &args.path {
        Some(path) => path.clone(),
        None => std::env::current_dir()?,
    };
    let canonical_root = root.canonicalize().unwrap_or(root);
    let options = args.scan_options();
    let metric = args.metric();

    let scanner = Scanner::new(mode, policy)?;

    // Targeted `--revalidate` paths invalidate before opening so they are
    // refreshed while untouched fresh siblings stay cached.
    if !args.revalidate_paths.is_empty() {
        let _ = scanner.invalidate(&HostKey::Local, &args.revalidate_paths)?;
    }

    // Bounded-memory streaming mode (`--stream` or `--mode stream`).
    if args.stream || matches!(args.storage_mode(), StorageMode::Streaming) {
        let mut stdout = io::BufWriter::new(io::stdout().lock());
        scanner.stream(&canonical_root, &options, None, |item| {
            write_ndjson_line(&mut stdout, &item)
        })?;
        stdout.flush()?;
        return Ok(());
    }

    let space = space_info(&canonical_root).ok();

    // Structured JSON mode (`--json`).
    if args.json {
        let snapshot = scanner.scan_blocking(
            &canonical_root,
            options,
            policy,
            args.stale_ok,
        )?;
        let doc = build_document(
            &snapshot,
            metric,
            options.apparent_size,
            options.include_hidden,
            args.depth,
            args.top,
            space,
        );
        let mut stdout = io::BufWriter::new(io::stdout().lock());
        write_pretty(&mut stdout, &doc)?;
        stdout.flush()?;
        return Ok(());
    }

    // NDJSON mode (`--ndjson`, optionally emitting stale then live with `--swr`).
    if args.ndjson {
        let session = scanner.open_with(
            HostKey::Local,
            canonical_root,
            options,
            policy,
        )?;
        let mut stdout = io::BufWriter::new(io::stdout().lock());
        while let Ok(event) = session.recv() {
            match event {
                ScanEvent::Snapshot(snap) => {
                    let is_final = matches!(
                        snap.freshness,
                        Freshness::Fresh { .. } | Freshness::Live { .. }
                    );
                    if args.swr || args.stale_ok || is_final {
                        let doc = build_document(
                            &snap,
                            metric,
                            options.apparent_size,
                            options.include_hidden,
                            args.depth,
                            args.top,
                            space,
                        );
                        write_ndjson_line(&mut stdout, &doc)?;
                        stdout.flush()?;
                    }
                    if args.stale_ok || is_final {
                        session.cancel();
                        break;
                    }
                }
                ScanEvent::Error { message } => {
                    eprintln!("disktree-cli: {message}");
                }
                ScanEvent::Finished => break,
                _ => {}
            }
        }
        return Ok(());
    }

    // Non-TTY stdout without `--json`: print a concise ranked summary table.
    if !io::stdout().is_terminal() {
        let snapshot = scanner.scan_blocking(
            &canonical_root,
            options,
            policy,
            args.stale_ok,
        )?;
        print_plain_summary(
            &snapshot,
            options.apparent_size,
            options.include_hidden,
            args.top.unwrap_or(20),
        )?;
        return Ok(());
    }

    // Interactive Ratatui TUI.
    let session = scanner.open_with(
        HostKey::Local,
        canonical_root.clone(),
        options,
        policy,
    )?;
    let exported = tui::run_tui(
        &session,
        &canonical_root,
        options,
        scanner.active_mode(),
        scanner.fallback_reason(),
        space,
    )?;

    if !exported.is_empty() {
        let mut stdout = io::BufWriter::new(io::stdout().lock());
        for path in exported {
            writeln!(stdout, "{}", path.display())?;
        }
        stdout.flush()?;
    }

    Ok(())
}

fn run_subcommand(
    command: &Command,
    mode: StorageMode,
    policy: disktree_scanner::CachePolicy,
) -> Result<()> {
    let scanner = Scanner::new(mode, policy)?;
    match command {
        Command::Invalidate { paths, json } => {
            let report = scanner.invalidate(&HostKey::Local, paths)?;
            if *json {
                #[derive(Serialize)]
                struct Out<'a> {
                    paths: &'a [PathBuf],
                    invalidated_dirs: u64,
                    touched_ancestors: u64,
                }
                let mut stdout = io::stdout().lock();
                serde_json::to_writer_pretty(
                    &mut stdout,
                    &Out {
                        paths,
                        invalidated_dirs: report.invalidated_dirs,
                        touched_ancestors: report.touched_ancestors,
                    },
                )?;
                writeln!(stdout)?;
            } else {
                println!(
                    "Invalidated {} dir(s) ({} ancestor(s) marked stale).",
                    report.invalidated_dirs, report.touched_ancestors
                );
            }
        }
        Command::Cache { action } => match action {
            CacheAction::Status { json } => {
                let stats = scanner.cache_stats()?;
                if *json {
                    let mut stdout = io::stdout().lock();
                    serde_json::to_writer_pretty(&mut stdout, &stats)?;
                    writeln!(stdout)?;
                } else {
                    println!("mode:         {:?}", scanner.active_mode());
                    if let Some(path) = &stats.db_path {
                        println!("db_path:      {}", path.display());
                    }
                    println!("db_size:      {}", human_bytes(stats.db_bytes));
                    println!("hosts:        {}", stats.hosts);
                    println!("indexed_dirs: {}", stats.indexed_dirs);
                    println!("chunks:       {}", stats.chunks);
                }
            }
            CacheAction::Clear { json } => {
                scanner.clear_cache()?;
                if *json {
                    println!(r#"{{"cleared":true}}"#);
                } else {
                    println!("Cache cleared.");
                }
            }
            CacheAction::Prune { older_than, json } => {
                let pruned = scanner.prune_cache(*older_than)?;
                if *json {
                    println!(r#"{{"pruned_chunks":{pruned}}}"#);
                } else {
                    println!("Pruned {pruned} expired chunk(s).");
                }
            }
        },
    }
    Ok(())
}

fn print_plain_summary(
    snapshot: &disktree_scanner::ScanSnapshot,
    apparent: bool,
    include_hidden: bool,
    top_n: usize,
) -> io::Result<()> {
    let arena = &snapshot.arena;
    let root = arena.node(arena.root());
    let total = if apparent {
        root.apparent_bytes
    } else {
        root.allocated_bytes
    };
    let now = disktree_scanner::unix_now();
    let freshness_label = match &snapshot.freshness {
        Freshness::Fresh { .. } => "fresh".to_owned(),
        Freshness::Stale { newest_at, .. } => {
            format!("stale {}", human_age(now.saturating_sub(*newest_at)))
        }
        Freshness::Partial { .. } => "partial".to_owned(),
        Freshness::Live { .. } => "live".to_owned(),
    };

    let mut stdout = io::BufWriter::new(io::stdout().lock());
    writeln!(
        stdout,
        "{}  {}  ({} files, {} dirs, {}, {} ms)",
        human_bytes(total),
        arena.root_path().display(),
        root.files,
        root.dirs,
        freshness_label,
        snapshot.stats.duration_ms,
    )?;

    for id in arena
        .children(arena.root())
        .filter(|&kid| include_hidden || !arena.node(kid).hidden())
        .take(top_n)
    {
        let node = arena.node(id);
        let bytes = if apparent {
            node.apparent_bytes
        } else {
            node.allocated_bytes
        };
        let suffix = if node.is_dir() { "/" } else { "" };
        writeln!(
            stdout,
            "  {:>10}  {}{suffix}",
            human_bytes(bytes),
            arena.name(id),
        )?;
    }
    stdout.flush()
}

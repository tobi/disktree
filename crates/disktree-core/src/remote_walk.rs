//! Bounded-memory SSH wire serializer backed by `disktree-scanner`'s streaming
//! walker. No Node, arena, child vector or sort: the receiver owns the tree;
//! the helper retains only its open ancestors.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use disktree_scanner::{
    NodeKind, StreamConfig, StreamItem, stream_walk_with_config,
};
use rustc_hash::FxHashSet;

use crate::remote::{Record, send};
use crate::scan::ScanOptions;

// Legacy peers require helper-side accounting. Modern clients explicitly
// negotiate desktop de-duplication and never populate this table.
const MAX_PENDING_LINKS: usize = 65_536;

pub fn stream(
    root: &Path,
    options: &ScanOptions,
    out: &mut impl Write,
    live_progress: bool,
    client_dedup: bool,
    live_tree: bool,
) -> io::Result<()> {
    if options.follow_links || !options.one_filesystem {
        return Err(io::Error::other(
            "Remote streaming scans never follow symlinks or cross mounts; \
             select that mount as a separate scan root.",
        ));
    }
    // Require a table, not just st_dev: a bind mount can have the same dev.
    let boundaries =
        crate::space::mount_boundaries_for(root).ok_or_else(|| {
            io::Error::other("cannot read remote mount boundaries")
        })?;
    let mut excluded: FxHashSet<PathBuf> = boundaries.into_iter().collect();
    excluded.extend(crate::space::never_scanned(root, root));
    stream_with_boundaries(
        root,
        options,
        &excluded,
        out,
        live_progress,
        client_dedup,
        live_tree,
    )
}

fn stream_with_boundaries(
    root: &Path,
    options: &ScanOptions,
    excluded: &FxHashSet<PathBuf>,
    out: &mut impl Write,
    live_progress: bool,
    client_dedup: bool,
    live_tree: bool,
) -> io::Result<()> {
    let scanner_opts = options.to_scanner();
    let config = StreamConfig {
        excluded: excluded.clone(),
        dedup_in_walker: !client_dedup,
        max_pending_links: (!client_dedup).then_some(MAX_PENDING_LINKS),
        skip_lossy_names: true,
    };
    let mut heartbeat = None::<Instant>;
    let maybe_heartbeat = |dir: &Path,
                           heartbeat: &mut Option<Instant>,
                           out: &mut dyn Write|
     -> io::Result<()> {
        if live_progress
            && heartbeat.is_none_or(|last| {
                last.elapsed() >= crate::remote_progress::INTERVAL
            })
        {
            serde_json::to_writer(
                &mut *out,
                &serde_json::json!({"directory": dir}),
            )
            .map_err(io::Error::other)?;
            out.write_all(b"\n")?;
            // Flush on a timer even when few tree records fill the buffer.
            out.flush()?;
            *heartbeat = Some(Instant::now());
        }
        Ok(())
    };

    stream_walk_with_config(root, &scanner_opts, &config, None, |item| {
        match item {
            StreamItem::EnterDir {
                path, name, depth, ..
            } => {
                maybe_heartbeat(&path, &mut heartbeat, out)?;
                if live_tree {
                    let wire_name = if depth == 0 { "" } else { name.as_str() };
                    send(out, &serde_json::json!({"enter": wire_name}))?;
                }
            }
            StreamItem::LeaveDir {
                name,
                depth,
                read_error,
                direct_children,
                ..
            } => {
                let wire_name =
                    if depth == 0 { "root".to_owned() } else { name };
                let record = Record(
                    wire_name,
                    0,
                    0,
                    0,
                    0,
                    read_error,
                    direct_children as usize,
                );
                send(out, &record)?;
            }
            StreamItem::Entry {
                path,
                name,
                kind,
                allocated_bytes,
                apparent_bytes,
                modified,
                hardlink,
                ..
            } => {
                if let Some(parent) = path.parent() {
                    maybe_heartbeat(parent, &mut heartbeat, out)?;
                }
                let wire_kind = match kind {
                    NodeKind::Directory => 0,
                    NodeKind::File => 1,
                    NodeKind::Symlink => 2,
                    NodeKind::Other => 3,
                };
                let bytes = if wire_kind == 3 {
                    0
                } else if options.apparent_size {
                    apparent_bytes
                } else {
                    allocated_bytes
                };
                let record = Record(
                    name,
                    wire_kind,
                    bytes,
                    u64::from(kind == NodeKind::File),
                    modified,
                    false,
                    0,
                );
                if client_dedup
                    && options.dedup_hardlinks
                    && let Some(link) = hardlink
                {
                    #[derive(serde::Serialize)]
                    struct Linked<'a> {
                        link: (u64, u64),
                        entry: &'a Record,
                    }
                    send(
                        out,
                        &Linked {
                            link,
                            entry: &record,
                        },
                    )?;
                } else {
                    send(out, &record)?;
                }
            }
            StreamItem::Error { .. } => {}
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn records(
        root: &Path,
        options: &ScanOptions,
        excluded: &[PathBuf],
    ) -> Vec<Record> {
        let mut bytes = Vec::new();
        stream_with_boundaries(
            root,
            options,
            &excluded.iter().cloned().collect(),
            &mut bytes,
            false,
            false,
            false,
        )
        .unwrap();
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect()
    }

    #[test]
    fn streaming_accounts_for_hardlinks_symlinks_and_allocated_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        fs::write(&data, b"hello").unwrap();
        fs::hard_link(&data, dir.path().join("alias")).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/leaf"), b"leaf").unwrap();
        std::os::unix::fs::symlink("sub", dir.path().join("link")).unwrap();
        let rows = records(dir.path(), &ScanOptions::default(), &[]);
        let actual: u64 =
            rows.iter().filter(|row| row.1 != 0).map(|row| row.2).sum();
        let expected =
            crate::scan::scan(dir.path(), ScanOptions::default()).unwrap();
        assert_eq!(actual, expected.bytes);
        assert_eq!(rows.iter().map(|row| row.3).sum::<u64>(), 3);
        assert_eq!(rows.iter().filter(|row| row.0 == "leaf").count(), 1);
        assert_eq!(rows.last().unwrap().6, 4);
        assert_eq!(rows.iter().filter(|row| row.0 == "sub").count(), 1);
    }

    #[test]
    fn mounted_children_are_omitted_even_on_the_same_device() {
        let dir = tempfile::tempdir().unwrap();
        let mounted = dir.path().join("mounted");
        fs::create_dir(&mounted).unwrap();
        fs::write(mounted.join("must-not-read"), b"secret").unwrap();
        fs::write(dir.path().join("keep"), b"hello").unwrap();
        let rows = records(dir.path(), &ScanOptions::default(), &[mounted]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "keep");
        assert_eq!(rows[1].6, 1);
    }

    #[test]
    fn hidden_and_depth_filters_do_not_change_postorder_counts() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".hidden"), b"hidden").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/leaf"), b"leaf").unwrap();
        fs::write(dir.path().join("visible"), b"visible").unwrap();
        let rows = records(
            dir.path(),
            &ScanOptions {
                include_hidden: false,
                max_depth: Some(0),
                ..ScanOptions::default()
            },
            &[],
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "visible");
        assert_eq!(rows[1].6, 1);
    }

    #[test]
    fn a_deep_directory_does_not_abort_the_entire_stream() {
        let dir = tempfile::tempdir().unwrap();
        let mut path = dir.path().to_path_buf();
        for _ in 0..320 {
            path.push("d");
            fs::create_dir(&path).unwrap();
        }
        fs::write(path.join("leaf"), b"found").unwrap();
        let rows = records(dir.path(), &ScanOptions::default(), &[]);
        assert_eq!(rows.iter().filter(|row| row.1 == 0).count(), 321);
        assert!(rows.iter().any(|row| row.0 == "leaf"));
        assert!(!rows.iter().any(|row| row.5));
    }

    #[test]
    fn unsafe_traversal_options_fail_before_walking() {
        let dir = tempfile::tempdir().unwrap();
        for options in [
            ScanOptions {
                follow_links: true,
                ..ScanOptions::default()
            },
            ScanOptions {
                one_filesystem: false,
                ..ScanOptions::default()
            },
        ] {
            assert!(
                stream(
                    dir.path(),
                    &options,
                    &mut Vec::new(),
                    false,
                    false,
                    false
                )
                .is_err()
            );
        }
    }
}

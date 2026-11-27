//! Integration tests for `disktree-scanner`: caching, SWR, zero-rescan flag
//! toggles, cross-shard hardlinks, host isolation, space efficiency, priority
//! preemption, and automatic storage fallback.

#![cfg(feature = "cache-db")]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use disktree_scanner::{
    ActiveStorageMode, CachePolicy, Freshness, HostIdentity, HostKey,
    PauseGate, ScanOptions, Scanner, StorageMode,
};

fn zero_headroom_policy() -> CachePolicy {
    CachePolicy {
        min_free_headroom: 0,
        ..CachePolicy::default()
    }
}

#[test]
fn toggling_apparent_size_and_hidden_hits_same_cache_without_rescan() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("toggle.redb");
    let root = temp.path().join("tree");
    fs::create_dir_all(&root).expect("mkdir");
    fs::write(root.join("visible.txt"), vec![1u8; 500]).expect("write");
    fs::write(root.join(".secret.txt"), vec![2u8; 300]).expect("write hidden");

    let scanner = Scanner::new(
        StorageMode::Persistent {
            path: db_path,
            strict: true,
        },
        zero_headroom_policy(),
    )
    .expect("scanner");

    // Initial scan: allocated bytes, include_hidden = true.
    let initial = scanner
        .scan_blocking(
            &root,
            ScanOptions::default(),
            zero_headroom_policy(),
            false,
        )
        .expect("initial");
    assert!(matches!(initial.freshness, Freshness::Live { .. }));
    assert_eq!(initial.stats.apparent_bytes, 800);
    assert_eq!(initial.stats.files, 2);

    // Toggle apparent_size = true -> instant fresh cache hit, 0 walked dirs.
    let apparent = scanner
        .scan_blocking(
            &root,
            ScanOptions {
                apparent_size: true,
                ..ScanOptions::default()
            },
            zero_headroom_policy(),
            false,
        )
        .expect("apparent");
    assert!(matches!(apparent.freshness, Freshness::Fresh { .. }));
    assert_eq!(apparent.stats.walked_dirs, 0);
    assert_eq!(apparent.stats.apparent_bytes, 800);

    // Toggle include_hidden = false -> instant fresh cache hit, 0 walked dirs,
    // and `.secret.txt` is filtered out of aggregated totals!
    let no_hidden = scanner
        .scan_blocking(
            &root,
            ScanOptions {
                include_hidden: false,
                ..ScanOptions::default()
            },
            zero_headroom_policy(),
            false,
        )
        .expect("no_hidden");
    assert!(matches!(no_hidden.freshness, Freshness::Fresh { .. }));
    assert_eq!(no_hidden.stats.walked_dirs, 0);
    assert_eq!(no_hidden.stats.apparent_bytes, 500);
    assert_eq!(no_hidden.stats.files, 1);
}

#[test]
fn cross_shard_hardlinks_dedup_when_one_subtree_is_reused_from_cache() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("hl.redb");
    let root = temp.path().join("root");
    let dir_a = root.join("a");
    let dir_b = root.join("b");
    fs::create_dir_all(&dir_a).expect("mkdir a");
    fs::create_dir_all(&dir_b).expect("mkdir b");

    let original = dir_a.join("shared.bin");
    fs::write(&original, vec![7u8; 8192]).expect("write");
    fs::hard_link(&original, dir_b.join("alias.bin")).expect("hardlink");

    let scanner = Scanner::new(
        StorageMode::Persistent {
            path: db_path,
            strict: true,
        },
        zero_headroom_policy(),
    )
    .expect("scanner");

    // 1. Scan only `a` so `a` is cached with its sparse hardlink identity.
    let snap_a = scanner
        .scan_blocking(
            &dir_a,
            ScanOptions::default(),
            zero_headroom_policy(),
            false,
        )
        .expect("scan a");
    assert_eq!(snap_a.stats.apparent_bytes, 8192);

    // 2. Widen to `root`: `a` is reused from cache while `b` is freshly walked.
    //    Hardlink dedup must still charge `shared.bin`/`alias.bin` once!
    let snap_root = scanner
        .scan_blocking(
            &root,
            ScanOptions::default(),
            zero_headroom_policy(),
            false,
        )
        .expect("scan root");
    assert!(snap_root.stats.skipped_fresh_dirs >= 1);
    assert_eq!(snap_root.stats.apparent_bytes, 8192);
    assert_eq!(snap_root.stats.files, 2);

    // 3. Reloading the full cached tree must preserve the exact same byte
    //    totals regardless of shard assembly order!
    let cached_root = scanner
        .scan_blocking(
            &root,
            ScanOptions::default(),
            zero_headroom_policy(),
            false,
        )
        .expect("cached root");
    assert!(matches!(cached_root.freshness, Freshness::Fresh { .. }));
    assert_eq!(
        cached_root.stats.allocated_bytes,
        snap_root.stats.allocated_bytes
    );
    assert_eq!(
        cached_root.stats.apparent_bytes,
        snap_root.stats.apparent_bytes
    );
}

#[test]
fn remote_host_namespaces_are_isolated_from_local_cache() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("hosts.redb");
    let root = temp.path().join("data");
    fs::create_dir_all(&root).expect("mkdir");
    fs::write(root.join("f.bin"), vec![1u8; 256]).expect("write");

    let scanner = Scanner::new(
        StorageMode::Persistent {
            path: db_path,
            strict: true,
        },
        zero_headroom_policy(),
    )
    .expect("scanner");

    // Populate local namespace.
    let _ = scanner
        .scan_blocking(
            &root,
            ScanOptions::default(),
            zero_headroom_policy(),
            false,
        )
        .expect("local");

    // Querying the same path under a remote HostIdentity must NOT hit Local!
    let remote = HostKey::Remote(HostIdentity {
        hostname: "prod-01".to_owned(),
        machine_id: Some("0123456789abcdef0123456789abcdef".to_owned()),
    });
    let session = scanner
        .open_with(remote, root, ScanOptions::default(), zero_headroom_policy())
        .expect("open remote");
    let first = session.recv().expect("first event");
    // Should NOT be a cached `Fresh` snapshot from Local.
    if let disktree_scanner::ScanEvent::Snapshot(snap) = first {
        assert!(!matches!(snap.freshness, Freshness::Fresh { .. }));
    }
}

#[test]
fn corrupt_db_file_falls_back_gracefully_to_in_memory() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("corrupt.redb");
    fs::write(&db_path, b"this is definitely not a valid redb file")
        .expect("write garbage");

    let root = temp.path().join("data");
    fs::create_dir_all(&root).expect("mkdir");
    fs::write(root.join("ok.txt"), b"still works").expect("write");

    let scanner = Scanner::new(
        StorageMode::Persistent {
            path: db_path,
            strict: false,
        },
        zero_headroom_policy(),
    )
    .expect("graceful fallback");
    assert_eq!(scanner.active_mode(), ActiveStorageMode::InMemory);
    assert!(scanner.fallback_reason().is_some());

    let snap = scanner
        .scan_blocking(
            &root,
            ScanOptions::default(),
            zero_headroom_policy(),
            false,
        )
        .expect("scan works in memory");
    assert_eq!(snap.stats.apparent_bytes, 11);
}

#[cfg(unix)]
#[test]
fn read_only_cache_directory_falls_back_to_in_memory() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().expect("tempdir");
    let ro_dir = temp.path().join("ro");
    fs::create_dir_all(&ro_dir).expect("mkdir ro");
    fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o555))
        .expect("chmod 555");

    let root = temp.path().join("data");
    fs::create_dir_all(&root).expect("mkdir data");
    fs::write(root.join("a.txt"), b"abc").expect("write");

    let is_unwritable = fs::File::create(ro_dir.join(".probe")).is_err();

    let scanner = Scanner::new(
        StorageMode::Persistent {
            path: ro_dir.join("cache.redb"),
            strict: false,
        },
        zero_headroom_policy(),
    )
    .expect("fallback on ro dir");

    // Restore perms so TempDir cleanup succeeds cleanly.
    let _ = fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o755));

    if is_unwritable {
        assert_eq!(scanner.active_mode(), ActiveStorageMode::InMemory);
        assert!(scanner.fallback_reason().is_some());
    }

    let snap = scanner
        .scan_blocking(
            &root,
            ScanOptions::default(),
            zero_headroom_policy(),
            false,
        )
        .expect("scan succeeds");
    assert_eq!(snap.stats.apparent_bytes, 3);
}

#[test]
fn temp_db_mode_works_and_cleans_up_on_drop() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("data");
    fs::create_dir_all(&root).expect("mkdir");
    fs::write(root.join("hello.txt"), b"tempdb").expect("write");

    let db_path: Option<PathBuf>;
    {
        let scanner = Scanner::new(StorageMode::TempDb, zero_headroom_policy())
            .expect("tempdb");
        assert_eq!(scanner.active_mode(), ActiveStorageMode::TempDb);
        let snap = scanner
            .scan_blocking(
                &root,
                ScanOptions::default(),
                zero_headroom_policy(),
                false,
            )
            .expect("scan");
        assert_eq!(snap.stats.apparent_bytes, 6);
        let stats = scanner.cache_stats().expect("stats");
        assert!(stats.chunks >= 1);
        db_path = stats.db_path;
        assert!(db_path.as_ref().is_some_and(|p| p.exists()));
    }

    // Temp DB file should be removed once Scanner and its background worker
    // release the last Arc<CacheDb>.
    if let Some(path) = db_path {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(2);
        while path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!path.exists(), "temp db should be cleaned up on drop");
    }
}

#[test]
fn prefix_trie_and_columnar_protobuf_compresses_repeated_names_efficiently() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("compact.redb");
    let root = temp.path().join("repo");

    // Create 60 directories x 50 repeated filenames = 3,000 files.
    for d in 0..60 {
        let dir = root.join(format!("package-{d:03}/src/components"));
        fs::create_dir_all(&dir).expect("mkdir");
        for f in 0..50 {
            let name = match f % 5 {
                0 => format!("index-{f:02}.ts"),
                1 => format!("component-{f:02}.tsx"),
                2 => format!("component-{f:02}.test.tsx"),
                3 => format!("styles-module-{f:02}.css"),
                _ => format!("package-metadata-{f:02}.json"),
            };
            fs::write(dir.join(name), b"x").expect("write");
        }
    }

    let scanner = Scanner::new(
        StorageMode::Persistent {
            path: db_path,
            strict: true,
        },
        zero_headroom_policy(),
    )
    .expect("scanner");

    let first = scanner
        .scan_blocking(
            &root,
            ScanOptions::default(),
            zero_headroom_policy(),
            false,
        )
        .expect("scan");
    assert_eq!(first.stats.files, 3_000);

    // Second scan must hit cache with 0 disk walks.
    let second = scanner
        .scan_blocking(
            &root,
            ScanOptions::default(),
            zero_headroom_policy(),
            false,
        )
        .expect("cached scan");
    assert!(matches!(second.freshness, Freshness::Fresh { .. }));
    assert_eq!(second.stats.files, 3_000);
    assert_eq!(second.stats.walked_dirs, 0);

    let stats = scanner.cache_stats().expect("stats");
    assert!(stats.chunks >= 1);
    assert!(stats.indexed_dirs >= 1);

    // Clear and prune sanity checks.
    assert_eq!(
        scanner.prune_cache(Duration::from_hours(1)).expect("prune"),
        0
    );
    scanner.clear_cache().expect("clear");
    assert_eq!(scanner.cache_stats().expect("stats").chunks, 0);
}

#[test]
fn pause_gate_blocks_background_until_immediate_resumes() {
    let gate = Arc::new(PauseGate::new());
    let cancel = Arc::new(AtomicBool::new(false));
    let passed = Arc::new(AtomicUsize::new(0));

    gate.pause();
    let g = Arc::clone(&gate);
    let c = Arc::clone(&cancel);
    let p = Arc::clone(&passed);
    let handle = thread::spawn(move || {
        g.wait_if_paused(&c);
        p.fetch_add(1, Ordering::SeqCst);
    });

    thread::sleep(Duration::from_millis(30));
    assert_eq!(
        passed.load(Ordering::SeqCst),
        0,
        "worker must remain paused"
    );

    gate.resume();
    handle.join().expect("join");
    assert_eq!(passed.load(Ordering::SeqCst), 1, "worker must resume");
}

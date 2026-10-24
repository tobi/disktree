//! Exercise the shipped executable, not just its protocol functions.
use std::io::{BufRead as _, BufReader, Write as _};
use std::process::{Command, Stdio};

#[cfg(unix)]
use disktree_core::remote::{Request, read_snapshot};
use disktree_core::scan::ScanOptions;

// The helper runs here, so this machine is the host, and clients only
// accept Unix hosts.
#[cfg(unix)]
#[test]
fn executable_returns_a_read_only_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("data");
    std::fs::write(&file, b"hello").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_disktree-scan"))
        .arg("--stdio-v1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let request = Request {
        root: Some(dir.path().into()),
        disk: false,
        options: ScanOptions {
            apparent_size: true,
            ..ScanOptions::default()
        },
    };
    let mut input = child.stdin.take().unwrap();
    serde_json::to_writer(&mut input, &request).unwrap();
    input.write_all(b"\n").unwrap();
    drop(input);
    let snapshot =
        read_snapshot(&mut BufReader::new(child.stdout.take().unwrap()))
            .unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(snapshot.tree.bytes, 5);
    assert_eq!(snapshot.tree.files, 1);
    assert_eq!(snapshot.header.root, dir.path().canonicalize().unwrap());
    assert_eq!(std::fs::read(file).unwrap(), b"hello");
}

// Every failure path still kills and reaps the child.
struct Helper(std::process::Child);

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn supervised_helper() -> Helper {
    Helper(
        Command::new(env!("CARGO_BIN_EXE_disktree-scan"))
            .arg("--stdio-v1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
}

fn wait_bounded(child: &mut std::process::Child) {
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "helper outlived its client"
        );
        std::thread::yield_now();
    }
}

#[test]
fn disconnect_stops_a_helper_even_when_its_output_is_blocked() {
    let dir = tempfile::tempdir().unwrap();
    // More records than a pipe can hold, without a large or expensive scan.
    for index in 0..2000 {
        std::fs::write(dir.path().join(format!("entry-{index:04}")), [])
            .unwrap();
    }
    let mut helper = supervised_helper();
    let mut input = helper.0.stdin.take().unwrap();
    let request = serde_json::json!({
        "root": dir.path(), "disk": false,
        "options": ScanOptions::default(), "watch_disconnect": true,
    });
    serde_json::to_writer(&mut input, &request).unwrap();
    input.write_all(b"\n").unwrap();
    let mut output = BufReader::new(helper.0.stdout.take().unwrap());
    let mut header = String::new();
    output.read_line(&mut header).unwrap();
    assert!(header.contains("\"version\""));
    // Leave stdout open and unread: only the stdin watchdog can stop it.
    drop(input);
    wait_bounded(&mut helper.0);
}

// The helper runs here, so this machine is the host, and clients only
// accept Unix hosts.
#[cfg(unix)]
#[test]
fn a_live_supervised_client_receives_a_complete_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("data"), b"hello").unwrap();
    let mut helper = supervised_helper();
    let mut input = helper.0.stdin.take().unwrap();
    let request = serde_json::json!({
        "root": dir.path(), "disk": false,
        "options": ScanOptions::default(), "watch_disconnect": true,
    });
    serde_json::to_writer(&mut input, &request).unwrap();
    input.write_all(b"\n").unwrap();
    let snapshot =
        read_snapshot(&mut BufReader::new(helper.0.stdout.take().unwrap()))
            .unwrap();
    assert_eq!(snapshot.tree.files, 1);
    assert!(helper.0.wait().unwrap().success());
    drop(input);
}

#[cfg(target_os = "linux")]
#[test]
fn memory_and_core_limits_are_installed_before_reading_a_request() {
    let mut helper = supervised_helper();
    // No startup chatter on stderr: wait for the kernel limit itself
    // while the helper is still blocked waiting for its first request.
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(2);
    let limits = loop {
        let limits =
            std::fs::read_to_string(format!("/proc/{}/limits", helper.0.id()))
                .unwrap();
        if limits.lines().any(|line| {
            line.starts_with("Max address space")
                && line
                    .split_whitespace()
                    .nth(3)
                    .and_then(|value| value.parse::<u64>().ok())
                    .is_some_and(|value| value <= 1 << 30)
        }) {
            break limits;
        }
        assert!(std::time::Instant::now() < deadline);
        assert!(helper.0.try_wait().unwrap().is_none());
        std::thread::yield_now();
    };
    let address = limits
        .lines()
        .find(|line| line.starts_with("Max address space"))
        .unwrap();
    let values: Vec<_> = address.split_whitespace().collect();
    assert!(values[3].parse::<u64>().unwrap() <= 1 << 30);
    assert!(values[4].parse::<u64>().unwrap() <= 1 << 30);
    let core = limits
        .lines()
        .find(|line| line.starts_with("Max core file size"))
        .unwrap();
    let values: Vec<_> = core.split_whitespace().collect();
    assert_eq!(values[4], "0");
    assert_eq!(values[5], "0");
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "creates 100,000 local empty files to measure helper RSS"]
fn wide_stream_has_bounded_resident_memory() {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..100_000 {
        std::fs::File::create(dir.path().join(format!("file-{index:06}")))
            .unwrap();
    }
    assert_bounded_stream_memory(dir.path());
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "creates 70,000 hardlinks to inodes outside the scanned fixture"]
fn external_hardlinks_have_bounded_resident_memory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    for index in 0..70_000 {
        let file = outside.join(format!("file-{index}"));
        std::fs::File::create(&file).unwrap();
        std::fs::hard_link(&file, root.join(format!("file-{index}"))).unwrap();
    }
    assert_bounded_stream_memory(&root);
}

#[cfg(target_os = "linux")]
fn assert_bounded_stream_memory(root: &std::path::Path) {
    let mut helper = supervised_helper();
    let mut input = helper.0.stdin.take().unwrap();
    serde_json::to_writer(
        &mut input,
        &serde_json::json!({
            "root": root, "disk": false,
            "options": ScanOptions::default(), "watch_disconnect": true,
            "live_progress": true, "client_dedup": true, "live_tree": true,
        }),
    )
    .unwrap();
    input.write_all(b"\n").unwrap();
    let mut output = BufReader::new(helper.0.stdout.take().unwrap());
    let mut line = String::new();
    for sample in 0..2 {
        for _ in 0..20_000 {
            line.clear();
            assert!(output.read_line(&mut line).unwrap() > 0);
        }
        // Backpressure keeps the helper alive here; samples do not retain
        // the received records or run any expensive remote filesystem.
        let status =
            std::fs::read_to_string(format!("/proc/{}/status", helper.0.id()))
                .unwrap();
        let rss = status
            .lines()
            .find(|line| line.starts_with("VmRSS:"))
            .unwrap();
        let kib = rss
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse::<u64>()
            .unwrap();
        eprintln!(
            "helper RSS after {} records: {kib} KiB",
            (sample + 1) * 20_000
        );
        assert!(kib < 16 * 1024, "{rss}");
    }
    std::io::copy(&mut output, &mut std::io::sink()).unwrap();
    assert!(helper.0.wait().unwrap().success());
    drop(input);
}

// The helper runs here, so this machine is the host, and clients only
// accept Unix hosts.
#[cfg(unix)]
#[test]
fn executable_discovers_roots_without_scanning_a_path() {
    let mut helper = supervised_helper();
    let mut input = helper.0.stdin.take().unwrap();
    input.write_all(b"{\"discover_version\":1,\"watch_disconnect\":true,\"root\":\"/missing-disktree-discovery-root\"}\n").unwrap();
    let roots = disktree_core::remote::read_roots(&mut BufReader::new(
        helper.0.stdout.take().unwrap(),
    ))
    .unwrap();
    assert!(roots.volumes.iter().all(|volume| volume.space.is_none()));
    assert!(helper.0.wait().unwrap().success());
    drop(input);
}

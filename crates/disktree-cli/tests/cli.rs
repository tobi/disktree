//! End-to-end tests invoking the compiled `disktree-cli` binary.

use std::fs;
use std::process::Command;

use serde_json::Value;

fn cli_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_disktree-cli"))
}

#[test]
fn json_mode_caches_within_15m_and_invalidate_refreshes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cache_dir = temp.path().join("cache");
    let tree_dir = temp.path().join("tree");
    let sub = tree_dir.join("sub");
    fs::create_dir_all(&sub).expect("mkdir");
    fs::write(sub.join("a.bin"), vec![1u8; 2048]).expect("write a");

    // 1. Cold run: live scan.
    let out1 = cli_bin()
        .args([
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "--json",
            "-a",
            tree_dir.to_str().unwrap(),
        ])
        .output()
        .expect("run 1");
    assert!(
        out1.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let doc1: Value = serde_json::from_slice(&out1.stdout).expect("json 1");
    assert_eq!(doc1["schema_version"], 1);
    assert_eq!(doc1["freshness"]["status"], "live");
    assert_eq!(doc1["tree"]["apparent_bytes"], 2048);

    // 2. Warm repeat run (default 15m TTL): fresh cache hit, 0 walked dirs!
    let out2 = cli_bin()
        .args([
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "--json",
            "-a",
            tree_dir.to_str().unwrap(),
        ])
        .output()
        .expect("run 2");
    assert!(out2.status.success());
    let doc2: Value = serde_json::from_slice(&out2.stdout).expect("json 2");
    assert_eq!(doc2["freshness"]["status"], "fresh");
    assert_eq!(doc2["stats"]["walked_dirs"], 0);
    assert_eq!(doc2["tree"]["apparent_bytes"], 2048);

    // 3. Delete file externally and run `disktree-cli invalidate`.
    fs::remove_file(sub.join("a.bin")).expect("rm");
    let inv = cli_bin()
        .args([
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "invalidate",
            "--json",
            sub.to_str().unwrap(),
        ])
        .output()
        .expect("invalidate");
    assert!(
        inv.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&inv.stderr)
    );

    // 4. Next run immediately reflects 0 bytes.
    let out3 = cli_bin()
        .args([
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "--json",
            "-a",
            tree_dir.to_str().unwrap(),
        ])
        .output()
        .expect("run 3");
    assert!(out3.status.success());
    let doc3: Value = serde_json::from_slice(&out3.stdout).expect("json 3");
    assert_eq!(doc3["tree"]["apparent_bytes"], 0);

    // 5. `cache status --json` and `cache clear --json`.
    let status = cli_bin()
        .args([
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "cache",
            "status",
            "--json",
        ])
        .output()
        .expect("cache status");
    assert!(status.status.success());
    let st: Value =
        serde_json::from_slice(&status.stdout).expect("status json");
    assert!(st["chunks"].as_u64().unwrap_or(0) >= 1);

    let clear = cli_bin()
        .args([
            "--cache-dir",
            cache_dir.to_str().unwrap(),
            "cache",
            "clear",
            "--json",
        ])
        .output()
        .expect("cache clear");
    assert!(clear.status.success());
}

#[test]
fn streaming_mode_emits_valid_ndjson_records() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::write(temp.path().join("hello.txt"), b"hello").expect("write");

    let out = cli_bin()
        .args(["--stream", temp.path().to_str().unwrap()])
        .output()
        .expect("stream");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let lines: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("ndjson line"))
        .collect();
    assert!(lines.iter().any(|v| v["event"] == "enter_dir"));
    assert!(
        lines
            .iter()
            .any(|v| v["event"] == "entry" && v["name"] == "hello.txt")
    );
    assert!(lines.iter().any(|v| v["event"] == "leave_dir"));
}

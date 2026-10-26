//! The MCP server, driven a message at a time as a client would.

use std::fs;

use disktree_agent::mcp::Server;
use serde_json::{Value, json};

/// Send `message`; return what the server wrote back.
fn send(server: &mut Server, message: &Value) -> Vec<Value> {
    let mut replies = Vec::new();
    server.handle_line(&message.to_string(), &mut |reply| {
        replies.push(reply.clone());
    });
    replies
}

fn request(
    server: &mut Server,
    id: u64,
    method: &str,
    params: &Value,
) -> Value {
    let mut replies = send(
        server,
        &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
    );
    let reply = replies.pop().expect("a reply");
    assert_eq!(reply["id"], id);
    assert_eq!(reply["jsonrpc"], "2.0");
    reply
}

fn call(server: &mut Server, id: u64, tool: &str, arguments: &Value) -> Value {
    let reply = request(
        server,
        id,
        "tools/call",
        &json!({ "name": tool, "arguments": arguments }),
    );
    reply["result"].clone()
}

fn scratch() -> (tempfile::TempDir, String) {
    let temp = tempfile::tempdir().expect("tempdir");
    for (path, bytes) in [
        ("big/disk.iso", 40_000),
        ("big/small.txt", 10),
        ("cache/blob", 5_000),
        ("loose.bin", 20_000),
    ] {
        let path = temp.path().join(path);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, vec![0_u8; bytes]).expect("write");
    }
    let root = dunce::canonicalize(temp.path())
        .expect("canonical")
        .to_str()
        .expect("utf-8")
        .to_owned();
    (temp, root)
}

#[test]
fn the_handshake_and_tool_list() {
    let mut server = Server::new(None);
    let reply = request(
        &mut server,
        1,
        "initialize",
        &json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" },
        }),
    );
    let result = &reply["result"];
    assert_eq!(result["protocolVersion"], "2025-06-18");
    assert_eq!(result["serverInfo"]["name"], "disktree");
    assert!(result["capabilities"]["tools"].is_object());
    assert!(
        result["instructions"]
            .as_str()
            .expect("instructions")
            .contains("read-only")
    );

    // An unknown revision gets the newest this server speaks.
    let reply = request(
        &mut server,
        2,
        "initialize",
        &json!({ "protocolVersion": "1999-01-01" }),
    );
    assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");

    // Notifications are never answered.
    assert!(
        send(
            &mut server,
            &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })
        )
        .is_empty()
    );

    let reply = request(&mut server, 3, "tools/list", &json!({}));
    let tools = reply["result"]["tools"].as_array().expect("tools");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        [
            "scan_path",
            "largest_files",
            "largest_dirs",
            "list_files",
            "reclaimable_space",
            "disk_summary"
        ]
    );
    for tool in tools {
        assert_eq!(
            tool["annotations"]["readOnlyHint"], true,
            "{}",
            tool["name"]
        );
        assert_eq!(tool["annotations"]["destructiveHint"], false);
        assert_eq!(tool["inputSchema"]["type"], "object");
        assert!(
            tool["description"]
                .as_str()
                .expect("description")
                .contains("read-only")
        );
    }
    let list_files = &tools[3]["inputSchema"]["properties"];
    for filter in [
        "min_size",
        "extensions",
        "older_than",
        "name",
        "category",
        "reclaim",
    ] {
        assert!(
            list_files.get(filter).is_some(),
            "list_files lacks {filter}"
        );
    }
    assert!(
        tools[2]["inputSchema"]["properties"]
            .get("extensions")
            .is_none()
    );

    let reply = request(&mut server, 4, "ping", &json!({}));
    assert_eq!(reply["result"], json!({}));
}

#[test]
fn scan_path_returns_the_cli_report() {
    let (temp, root) = scratch();
    let mut server = Server::new(None);
    let result = call(
        &mut server,
        7,
        "scan_path",
        &json!({ "path": root, "apparent_size": true, "top": 2 }),
    );
    assert_eq!(result["isError"], false);
    let report = &result["structuredContent"];
    let text: Value = serde_json::from_str(
        result["content"][0]["text"].as_str().expect("text"),
    )
    .expect("the text is the same JSON");
    assert_eq!(&text, report);
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["total"]["bytes"], 65_010);
    assert_eq!(report["largest_files"][0]["name"], "disk.iso");
    assert_eq!(report["largest_dirs"][0]["name"], "big");
    assert_eq!(report["largest_files"].as_array().expect("files").len(), 2);

    // The command line prints the same lists for the same question.
    let args: Vec<std::ffi::OsString> =
        [root.as_str(), "--json", "-a", "-n", "2"]
            .iter()
            .map(Into::into)
            .collect();
    let mut out = Vec::new();
    assert_eq!(
        disktree_agent::cli::run("scan", &args, &mut out, &mut Vec::new()),
        0
    );
    let printed: Value = serde_json::from_slice(&out).expect("json");
    for key in [
        "root",
        "total",
        "largest_dirs",
        "largest_files",
        "options",
        "filters",
    ] {
        assert_eq!(printed[key], report[key], "{key}");
    }
    drop(temp);
}

#[test]
fn each_tool_returns_its_sections_and_filters_apply() {
    let (_temp, root) = scratch();
    let mut server = Server::new(None);
    let files = call(
        &mut server,
        1,
        "largest_files",
        &json!({ "path": root, "apparent_size": true }),
    );
    let report = &files["structuredContent"];
    assert!(report.get("largest_dirs").is_none());
    assert_eq!(report["largest_files"][0]["name"], "disk.iso");

    let dirs = call(
        &mut server,
        2,
        "largest_dirs",
        &json!({ "path": root, "depth": 1 }),
    );
    let report = &dirs["structuredContent"];
    assert!(report.get("largest_files").is_none());
    assert_eq!(report["largest_dirs"][0]["name"], "big");

    let listed = call(
        &mut server,
        3,
        "list_files",
        &json!({ "path": root, "apparent_size": true, "min_size": "10K", "extensions": "iso,bin" }),
    );
    let report = &listed["structuredContent"];
    let names: Vec<&str> = report["largest_files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| file["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["disk.iso", "loose.bin"]);
    assert_eq!(report["options"]["top"], 100);
    assert_eq!(report["matched"]["file_bytes"], 60_000);

    let summary =
        call(&mut server, 4, "disk_summary", &json!({ "path": root }));
    assert!(
        summary["structuredContent"]["capacity"]["bytes"]
            .as_u64()
            .expect("capacity")
            > 0
    );
}

#[test]
fn a_repeated_question_is_answered_from_the_cache() {
    let (temp, root) = scratch();
    let mut server = Server::new(None);
    let first = call(&mut server, 1, "scan_path", &json!({ "path": root }));
    fs::write(temp.path().join("late.bin"), vec![0_u8; 100_000])
        .expect("write");
    let cached =
        call(&mut server, 2, "largest_files", &json!({ "path": root }));
    assert_eq!(
        first["structuredContent"]["scanned_at"],
        cached["structuredContent"]["scanned_at"]
    );
    assert_ne!(
        cached["structuredContent"]["largest_files"][0]["name"],
        "late.bin"
    );
    let fresh = call(
        &mut server,
        3,
        "largest_files",
        &json!({ "path": root, "refresh": true }),
    );
    assert_eq!(
        fresh["structuredContent"]["largest_files"][0]["name"],
        "late.bin"
    );
}

#[test]
fn mistakes_are_reported_not_fatal() {
    let (temp, root) = scratch();
    // Absolute on every platform, unlike `/no/such`, which Windows reads as
    // relative to the current drive.
    let missing = temp
        .path()
        .join("missing")
        .to_str()
        .expect("utf-8")
        .to_owned();
    let mut server = Server::new(None);
    for (tool, arguments, needle) in [
        ("scan_path", json!({}), "path is required"),
        ("scan_path", json!({ "path": "relative/dir" }), "absolute"),
        ("scan_path", json!({ "path": missing }), "cannot read"),
        ("scan_path", json!({ "path": root, "top": 0 }), "top"),
        (
            "scan_path",
            json!({ "path": root, "min_size": "huge" }),
            "size",
        ),
        (
            "list_files",
            json!({ "path": root, "older_than": "soon" }),
            "age",
        ),
        (
            "largest_dirs",
            json!({ "path": root, "extensions": ["iso"] }),
            "takes no argument",
        ),
        (
            "largest_dirs",
            json!({ "path": root, "category": "junk" }),
            "category",
        ),
        (
            "largest_files",
            json!({ "path": root, "delete": true }),
            "takes no argument",
        ),
    ] {
        let result = call(&mut server, 9, tool, &arguments);
        assert_eq!(result["isError"], true, "{tool} {arguments}");
        let text = result["content"][0]["text"].as_str().expect("text");
        assert!(text.contains(needle), "{tool} {arguments}: {text}");
    }

    let reply = request(
        &mut server,
        10,
        "tools/call",
        &json!({ "name": "delete_files", "arguments": {} }),
    );
    assert_eq!(reply["error"]["code"], -32602);
    let reply = request(&mut server, 11, "resources/list", &json!({}));
    assert_eq!(reply["error"]["code"], -32601);

    let mut replies = Vec::new();
    server.handle_line("{not json", &mut |reply| replies.push(reply.clone()));
    assert_eq!(replies[0]["error"]["code"], -32700);
    assert_eq!(replies[0]["id"], Value::Null);
}

#[test]
fn no_tool_can_change_files() {
    let mut server = Server::new(None);
    let reply = request(&mut server, 1, "tools/list", &json!({}));
    for tool in reply["result"]["tools"].as_array().expect("tools") {
        let name = tool["name"].as_str().expect("name");
        for verb in [
            "delete", "remove", "trash", "move", "rename", "write", "clean",
        ] {
            assert!(!name.contains(verb), "{name}");
        }
    }
}

#[test]
fn an_expired_scan_is_let_go_while_idle() {
    use std::sync::mpsc;
    use std::time::Duration;

    let (_temp, root) = scratch();
    let (sender, inbox) = mpsc::channel();
    let (replies, received) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut server =
            Server::new(Some(inbox)).with_cache_for(Duration::from_millis(200));
        server.run(&mut |reply| {
            let _ = replies.send(reply.clone());
        });
        server
    });
    let call = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "scan_path", "arguments": { "path": root } },
    });
    sender.send(call.to_string()).expect("send");
    let reply = received
        .recv_timeout(Duration::from_secs(30))
        .expect("reply");
    assert_eq!(reply["result"]["isError"], false);
    // Nothing more is asked; the scan expires on its own.
    std::thread::sleep(Duration::from_millis(600));
    drop(sender);
    let server = server.join().expect("server");
    assert!(!server.has_cached_scan());
}

#[test]
fn reclaimable_space_returns_only_the_reclaimable_summary() {
    let (_temp, root) = scratch();
    let mut server = Server::new(None);
    let result = call(
        &mut server,
        1,
        "reclaimable_space",
        &json!({ "path": root, "apparent_size": true }),
    );
    assert_eq!(result["isError"], false);
    let report = &result["structuredContent"];
    let reclaimable = &report["reclaimable"];
    assert_eq!(reclaimable["bytes"], 5_000);
    assert_eq!(reclaimable["by_reason"][0]["reason"], "regenerable");
    assert_eq!(reclaimable["largest"][0]["name"], "cache");
    assert!(report["volume"]["available"]["bytes"].as_u64().is_some());
    for absent in ["breakdown", "largest_dirs", "largest_files"] {
        assert!(report.get(absent).is_none(), "{absent}");
    }
    // It describes the whole tree, so listing filters are refused.
    let result = call(
        &mut server,
        2,
        "reclaimable_space",
        &json!({ "path": root, "min_size": "1G" }),
    );
    assert_eq!(result["isError"], true);
}

/// A cancellation for a request still waiting behind a running scan is
/// kept, and that request is skipped with no reply.
#[test]
fn a_queued_request_can_be_cancelled() {
    use std::sync::mpsc;

    let (_temp, root) = scratch();
    let (sender, inbox) = mpsc::channel();
    let scan = |id: u64| {
        json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": "scan_path", "arguments": { "path": root } },
        })
        .to_string()
    };
    // All of it is waiting before the first scan starts: the scan reads
    // the rest while it runs.
    sender.send(scan(1)).expect("send");
    sender.send(scan(2)).expect("send");
    sender
        .send(
            json!({
                "jsonrpc": "2.0", "method": "notifications/cancelled",
                "params": { "requestId": 2 },
            })
            .to_string(),
        )
        .expect("send");
    sender
        .send(
            json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" }).to_string(),
        )
        .expect("send");
    drop(sender);
    let mut replies = Vec::new();
    Server::new(Some(inbox)).run(&mut |reply| replies.push(reply.clone()));
    let ids: Vec<&Value> = replies.iter().map(|reply| &reply["id"]).collect();
    assert_eq!(ids, [&json!(1), &json!(3)]);
}

/// A cancellation that arrives before its request means the request is
/// never answered.
#[test]
fn a_request_cancelled_in_advance_is_not_answered() {
    let mut server = Server::new(None);
    assert!(
        send(
            &mut server,
            &json!({
                "jsonrpc": "2.0", "method": "notifications/cancelled",
                "params": { "requestId": "a" },
            })
        )
        .is_empty()
    );
    assert!(
        send(
            &mut server,
            &json!({ "jsonrpc": "2.0", "id": "a", "method": "ping" })
        )
        .is_empty()
    );
    // Only once: the next request with that id is answered.
    assert_eq!(
        send(
            &mut server,
            &json!({ "jsonrpc": "2.0", "id": "a", "method": "ping" })
        )
        .len(),
        1
    );
}

#[test]
fn malformed_messages_are_invalid_requests() {
    let mut server = Server::new(None);
    for (message, id) in [
        (json!({}), Value::Null),
        (
            json!({ "jsonrpc": "2.0", "id": true, "method": "ping" }),
            Value::Null,
        ),
        (
            json!({ "jsonrpc": "2.0", "id": [1], "method": "ping" }),
            Value::Null,
        ),
        (
            json!({ "jsonrpc": "1.0", "id": 4, "method": "ping" }),
            json!(4),
        ),
        (json!({ "id": 5, "method": "ping" }), json!(5)),
        (json!({ "jsonrpc": "2.0", "id": 6 }), json!(6)),
    ] {
        let replies = send(&mut server, &message);
        assert_eq!(replies.len(), 1, "{message}");
        assert_eq!(replies[0]["error"]["code"], -32600, "{message}");
        assert_eq!(replies[0]["id"], id, "{message}");
    }
    // A response to something the server never asked is ignored.
    assert!(
        send(
            &mut server,
            &json!({ "jsonrpc": "2.0", "id": 7, "result": {} })
        )
        .is_empty()
    );
}

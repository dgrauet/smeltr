//! End-to-end: spawn `smeltr mcp` and exchange JSON-RPC frames over stdio.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn smeltr_path() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    while p.file_name().map(|n| n != "deps").unwrap_or(true) {
        p.pop();
    }
    p.pop();
    p.join("smeltr")
}

fn write_line<W: Write>(w: &mut W, line: &str) {
    w.write_all(line.as_bytes()).unwrap();
    w.write_all(b"\n").unwrap();
    w.flush().unwrap();
}

/// Reads one JSON line from the server. Skips empty lines.
fn read_json_line<R: BufRead>(r: &mut R, deadline: Instant) -> Option<serde_json::Value> {
    while Instant::now() < deadline {
        let mut line = String::new();
        match r.read_line(&mut line) {
            Ok(0) => return None,
            Ok(_) => {
                let t = line.trim();
                if t.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(t) {
                    return Some(v);
                }
                // Not JSON, treat as log noise — continue.
            }
            Err(_) => return None,
        }
    }
    None
}

#[test]
#[serial_test::serial]
fn mcp_stdio_initialize_then_list_tools() {
    let _ = Command::new("cargo")
        .args(["build", "-p", "smeltr-cli"])
        .status();

    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(smeltr_path())
        .env("SMELTR_HOME", home.path())
        .env("RUST_LOG", "warn")
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn smeltr mcp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    // Initialize request
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smeltr-test","version":"0.1"}}}"#;
    write_line(&mut stdin, init);

    let deadline = Instant::now() + Duration::from_secs(5);
    let init_resp = read_json_line(&mut reader, deadline).expect("no initialize response");
    assert_eq!(
        init_resp.get("id").and_then(|v| v.as_i64()),
        Some(1),
        "unexpected init resp: {init_resp}"
    );
    assert!(
        init_resp.get("result").is_some(),
        "expected result in init resp: {init_resp}"
    );

    // notifications/initialized (no id, no response)
    let initialized = r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;
    write_line(&mut stdin, initialized);

    // tools/list
    let list = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
    write_line(&mut stdin, list);

    let deadline = Instant::now() + Duration::from_secs(5);
    let list_resp = read_json_line(&mut reader, deadline).expect("no tools/list response");
    assert_eq!(
        list_resp.get("id").and_then(|v| v.as_i64()),
        Some(2),
        "unexpected list resp: {list_resp}"
    );

    // The response should contain { result: { tools: [{ name: "list_sessions" }, ...] } }
    let result = list_resp.get("result").expect("no result");
    let tools = result
        .get("tools")
        .and_then(|t| t.as_array())
        .unwrap_or_else(|| panic!("no tools array: {list_resp}"));
    let names: Vec<String> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();

    for want in [
        "list_sessions",
        "get_session_summary",
        "query_events",
        "find_correlations",
        "get_crash_report",
        "get_metal_cb_history",
        "compare_sessions",
    ] {
        assert!(
            names.iter().any(|n| n == want),
            "missing tool {want:?}, got: {names:?}"
        );
    }

    // Each tool must have a non-trivial inputSchema with at least one property
    // (except list_sessions which legitimately takes no params).
    for tool in tools.iter() {
        let name = tool.get("name").and_then(|n| n.as_str()).unwrap();
        let schema = tool.get("inputSchema").expect("tool has no inputSchema");
        let props = schema.get("properties").and_then(|p| p.as_object());
        if name == "list_sessions" {
            continue;
        }
        let props = props.unwrap_or_else(|| panic!("tool {name} has no properties"));
        assert!(
            !props.is_empty(),
            "tool {name} inputSchema has empty properties — placeholder not replaced"
        );
    }

    // Clean shutdown.
    drop(stdin);
    let exit_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < exit_deadline {
        match child.try_wait().unwrap() {
            Some(_) => break,
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

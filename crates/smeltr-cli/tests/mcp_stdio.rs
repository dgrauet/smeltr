//! End-to-end: spawn `smeltr mcp` and exchange JSON-RPC frames over stdio.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
                // stdout carries JSON-RPC only; anything else is a bug
                // (see `mcp_stdout_carries_only_json_rpc_while_logging`).
                return Some(
                    serde_json::from_str(t)
                        .unwrap_or_else(|e| panic!("non-JSON line on the MCP stdout: {t:?} ({e})")),
                );
            }
            Err(_) => return None,
        }
    }
    None
}

#[test]
#[serial_test::serial]
fn mcp_stdio_initialize_then_list_tools() {
    // `smeltr` is a bin in this crate: Cargo builds it before this integration
    // test and exposes it via CARGO_BIN_EXE_smeltr. (Do NOT shell out to
    // `cargo build` here — a nested cargo deadlocks on the outer build lock.)
    let home = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_smeltr"))
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

/// stdout is the JSON-RPC channel: nothing else may be written to it. A
/// session still being recorded makes the reader warn ("zstd stream not
/// sealed"), and that warning used to land on stdout, between frames.
#[test]
#[serial_test::serial]
fn mcp_stdout_carries_only_json_rpc_while_logging() {
    use smeltr_core::event::{Event, Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;

    let home = tempfile::tempdir().unwrap();
    std::env::set_var("SMELTR_HOME", home.path());
    // An open (unfinalized) session with one event: reading it warns.
    let mut w = SessionWriter::create(SessionMetadata::now_starting(SessionId::new())).unwrap();
    w.write_event(&Event {
        ts_mono_ns: 1,
        ts_wall_ns: 1,
        session_id: uuid::Uuid::nil(),
        source: Source::Mark,
        pid: None,
        seq: 1,
        payload: Payload::Mark {
            label: "open".into(),
            fields: Default::default(),
        },
    })
    .unwrap();
    w.flush().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_smeltr"))
        .env("SMELTR_HOME", home.path())
        .env("RUST_LOG", "warn")
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
    );
    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#,
    );
    write_line(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_sessions","arguments":{"include_empty":true}}}"#,
    );

    let mut foreign = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut answered = false;
    while Instant::now() < deadline && !answered {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(t) {
            Ok(v) => answered = v.get("id").and_then(|i| i.as_i64()) == Some(2),
            Err(_) => foreign.push(t.to_string()),
        }
    }
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    drop(w);

    assert!(answered, "no list_sessions response");
    assert!(
        foreign.is_empty(),
        "non-JSON-RPC output on stdout: {foreign:?}"
    );
}

//! End-to-end: a served MCP connection emits notifications/resources/list_changed
//! when a new session directory appears. Uses an in-process duplex transport
//! (newline-delimited JSON-RPC — the stdio framing) — no HTTP needed.

use serial_test::serial;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}"#;
const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

#[tokio::test]
#[serial]
async fn list_changed_fires_when_a_session_appears() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("SMELTR_HOME", home.path());

    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let (srv_read, srv_write) = tokio::io::split(server_io);
    let server = tokio::spawn(async move {
        use rmcp::ServiceExt;
        let service = smeltr_mcp::server::SmeltrMcpServer
            .serve((srv_read, srv_write))
            .await
            .expect("serve duplex");
        let _ = service.waiting().await;
    });

    let (cli_read, mut cli_write) = tokio::io::split(client_io);
    let mut lines = BufReader::new(cli_read).lines();

    cli_write.write_all(INITIALIZE.as_bytes()).await.unwrap();
    cli_write.write_all(b"\n").await.unwrap();
    let resp = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("initialize response in time")
        .unwrap()
        .expect("one line");
    assert!(resp.contains("smeltr-mcp"), "init response: {resp}");

    cli_write.write_all(INITIALIZED.as_bytes()).await.unwrap();
    cli_write.write_all(b"\n").await.unwrap();

    // Give the watcher a beat to take its baseline snapshot, then create a
    // session on disk.
    tokio::time::sleep(Duration::from_millis(300)).await;
    {
        use smeltr_core::session::{SessionId, SessionMetadata};
        use smeltr_core::writer::SessionWriter;
        let meta = SessionMetadata::now_starting(SessionId::new());
        let w = SessionWriter::create(meta).unwrap();
        drop(w);
    }

    // The 2 s watcher must notice within a generous deadline.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let line = tokio::time::timeout_at(deadline, lines.next_line())
            .await
            .expect("list_changed before deadline")
            .unwrap()
            .expect("stream open");
        if line.contains("notifications/resources/list_changed") {
            break;
        }
    }

    server.abort();
    std::env::remove_var("SMELTR_HOME");
}

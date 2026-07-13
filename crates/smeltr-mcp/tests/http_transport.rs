//! Real Streamable-HTTP handshake against `serve_on`, no mocks: bind an
//! ephemeral loopback port, POST a JSON-RPC `initialize` to /mcp with a
//! hand-written HTTP/1.1 request, and assert the server identifies itself.

#![cfg(feature = "http")]

use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}}"#;

#[tokio::test]
async fn initialize_over_http_returns_server_info() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(smeltr_mcp::http::serve_on(listener));

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {len}\r\nConnection: keep-alive\r\n\r\n{body}",
        port = addr.port(),
        len = INITIALIZE.len(),
        body = INITIALIZE,
    );
    stream.write_all(req.as_bytes()).await.unwrap();

    // Stateful streamable-HTTP answers with an SSE stream; read until the
    // InitializeResult (containing our server name) shows up.
    let mut buf = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut chunk = [0u8; 4096];
    let text = loop {
        let n = tokio::time::timeout_at(deadline, stream.read(&mut chunk))
            .await
            .expect("timed out waiting for initialize response")
            .unwrap();
        assert!(n > 0, "connection closed before initialize response");
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf).into_owned();
        if text.contains("smeltr-mcp") {
            break text;
        }
    };
    assert!(text.starts_with("HTTP/1.1 200"), "response was:\n{text}");
    assert!(text.contains("smeltr-mcp"), "response was:\n{text}");

    server.abort();
}

#[tokio::test]
async fn run_http_refuses_non_loopback() {
    let err = smeltr_mcp::http::run_http("0.0.0.0:8848".parse().unwrap())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("loopback"), "{err}");
}

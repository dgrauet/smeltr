//! End-to-end: spawn smeltrd, emit a synthetic watchdog event stream via the
//! socket, stop the daemon, and run `smeltr analyze --last`. Verify report.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::Duration;

fn write_frame<W: Write>(w: &mut W, value: &serde_json::Value) {
    let mut buf = Vec::new();
    ciborium::into_writer(value, &mut buf).unwrap();
    let len = (buf.len() as u32).to_le_bytes();
    w.write_all(&len).unwrap();
    w.write_all(&buf).unwrap();
}

fn read_frame<R: Read>(r: &mut R) -> ciborium::Value {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).unwrap();
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).unwrap();
    ciborium::from_reader(&buf[..]).unwrap()
}

fn kind_of(v: &ciborium::Value) -> Option<&str> {
    let map = v.as_map()?;
    for (k, val) in map {
        if k.as_text() == Some("kind") {
            return val.as_text();
        }
    }
    None
}

mod common;
use common::{smeltrd_path, DaemonGuard};

#[test]
#[serial_test::serial]
fn analyze_after_daemon_records_synthetic_watchdog() {
    let home = tempfile::tempdir().unwrap();
    let sock = home.path().join("sm.sock");

    let smeltrd = smeltrd_path();
    let mut daemon = DaemonGuard::new(
        Command::new(&smeltrd)
            .env("SMELTR_HOME", home.path())
            .env("SMELTR_SOCKET", &sock)
            .env("RUST_LOG", "warn")
            .arg("--foreground")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn smeltrd"),
    );

    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(sock.exists(), "smeltrd did not create socket");

    // Connect, Hello, then Emit the 7 fixture events.
    let mut stream = UnixStream::connect(&sock).unwrap();
    write_frame(
        &mut stream,
        &serde_json::json!({ "op": "Hello", "client": "e2e-analyze" }),
    );
    let welcome = read_frame(&mut stream);
    assert_eq!(kind_of(&welcome), Some("Welcome"));

    let fixture = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../smeltr-analyzer/fixtures/synthetic-watchdog.json"
    ))
    .unwrap();
    let events: serde_json::Value = serde_json::from_str(&fixture).unwrap();
    for ev in events.as_array().unwrap() {
        let payload = &ev["payload"];
        let source = ev["source"].as_str().unwrap();
        let pid = ev["pid"].clone();
        write_frame(
            &mut stream,
            &serde_json::json!({
                "op": "Emit",
                "source": source,
                "pid": if pid.is_null() { serde_json::Value::Null } else { pid },
                "payload": payload,
            }),
        );
        let _ack = read_frame(&mut stream);
    }
    drop(stream);

    // Give the daemon a moment to publish + run the trigger task (which writes
    // a post-mortem session).
    std::thread::sleep(Duration::from_millis(500));

    // Stop the daemon gracefully via a fresh connection.
    let mut stream = UnixStream::connect(&sock).unwrap();
    write_frame(
        &mut stream,
        &serde_json::json!({ "op": "Hello", "client": "e2e-stop" }),
    );
    let _ = read_frame(&mut stream);
    write_frame(&mut stream, &serde_json::json!({ "op": "Shutdown" }));
    let _ = read_frame(&mut stream);
    drop(stream);

    // Wait for daemon exit (up to 10s); the guard reaps whatever remains.
    for _ in 0..50 {
        match daemon.child_mut().and_then(|c| c.try_wait().ok().flatten()) {
            Some(_) => break,
            None => std::thread::sleep(Duration::from_millis(200)),
        }
    }
    drop(daemon);

    // Run `smeltr analyze --last`.
    let smeltr_bin = env!("CARGO_BIN_EXE_smeltr");
    let out = Command::new(smeltr_bin)
        .env("SMELTR_HOME", home.path())
        .args(["analyze", "--last"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "analyze failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ImpactingInteractivity"),
        "missing ImpactingInteractivity in:\n{}",
        stdout
    );
    assert!(stdout.contains("Queue depth peaked"));
    assert!(stdout.contains("ReportCrash"));
    assert!(stdout.contains("call_id=1"));
}

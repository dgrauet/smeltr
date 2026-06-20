//! Verify the reusable bus client forwards EventNotification events to its channel.

use smeltr_core::codec::{read_frame, write_frame};
use smeltr_core::event::{Payload, Source};
use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};
use std::process::Stdio;
use std::time::Duration;

fn smeltrd_path() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    while p.file_name().map(|n| n != "deps").unwrap_or(true) {
        p.pop();
    }
    p.pop();
    p.join("smeltrd")
}

#[tokio::test]
#[serial_test::serial]
async fn subscribe_events_forwards_bus_events() {
    let _ = std::process::Command::new("cargo")
        .args(["build", "-p", "smeltr-daemon"])
        .status();
    let home = tempfile::tempdir().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let sock = sock_dir.path().join("sm.sock");

    let mut daemon = std::process::Command::new(smeltrd_path())
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .env("RUST_LOG", "warn")
        .arg("--foreground")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(sock.exists(), "daemon did not create socket");

    // Subscriber via the shared helper.
    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
    let sock2 = sock.clone();
    let task = tokio::spawn(async move {
        let _ = smeltr_daemon::client::subscribe_events(&sock2, "test-sub", tx).await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await; // let it handshake before we emit (live-only)

    // Emitter (sync) on a blocking thread.
    let sock3 = sock.clone();
    let emitter = std::thread::spawn(move || {
        let mut s = std::os::unix::net::UnixStream::connect(&sock3).unwrap();
        write_frame(
            &mut s,
            &ClientToDaemon::Hello {
                client: "emit".into(),
            },
        )
        .unwrap();
        let _w: DaemonToClient = read_frame(&mut s).unwrap().unwrap();
        for i in 0..3 {
            write_frame(
                &mut s,
                &ClientToDaemon::Emit {
                    source: Source::Mark,
                    pid: None,
                    scope_token: None,
                    payload: Payload::Mark {
                        label: format!("e-{i}"),
                        fields: Default::default(),
                    },
                },
            )
            .unwrap();
            let _a: DaemonToClient = read_frame(&mut s).unwrap().unwrap();
        }
    });

    let mut seen = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while seen < 3 {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(ev)) => {
                if let Payload::Mark { ref label, .. } = ev.payload {
                    if label.starts_with("e-") {
                        seen += 1;
                    }
                }
            }
            _ => break,
        }
    }
    assert_eq!(seen, 3, "helper did not forward bus events");

    emitter.join().unwrap();
    task.abort();
    let _ = daemon.kill();
    let _ = daemon.wait();
}

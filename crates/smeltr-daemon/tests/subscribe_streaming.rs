//! Verify SubscribeEvents streams bus events until the client disconnects.

use smeltr_core::codec::{read_frame, write_frame};
use smeltr_core::event::{Payload, Source};
use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::Duration;

fn smeltrd_path() -> std::path::PathBuf {
    let mut p = std::env::current_exe().unwrap();
    while p.file_name().map(|n| n != "deps").unwrap_or(true) {
        p.pop();
    }
    p.pop();
    p.join("smeltrd")
}

#[test]
#[serial_test::serial]
fn subscribe_receives_emitted_events() {
    let _ = Command::new("cargo")
        .args(["build", "-p", "smeltr-daemon"])
        .status();
    let home = tempfile::tempdir().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let sock = sock_dir.path().join("sm.sock");

    let mut daemon = Command::new(smeltrd_path())
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

    // Subscriber connection.
    let mut sub_stream = UnixStream::connect(&sock).unwrap();
    sub_stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write_frame(
        &mut sub_stream,
        &ClientToDaemon::Hello {
            client: "sub".into(),
        },
    )
    .unwrap();
    let _welcome: DaemonToClient = read_frame(&mut sub_stream).unwrap().unwrap();
    write_frame(&mut sub_stream, &ClientToDaemon::SubscribeEvents).unwrap();
    let ack: DaemonToClient = read_frame(&mut sub_stream).unwrap().unwrap();
    assert!(
        matches!(ack, DaemonToClient::Ack),
        "expected Ack, got {ack:?}"
    );

    // Emitter connection.
    let mut emit_stream = UnixStream::connect(&sock).unwrap();
    write_frame(
        &mut emit_stream,
        &ClientToDaemon::Hello {
            client: "emit".into(),
        },
    )
    .unwrap();
    let _welcome: DaemonToClient = read_frame(&mut emit_stream).unwrap().unwrap();
    for i in 0..3 {
        write_frame(
            &mut emit_stream,
            &ClientToDaemon::Emit {
                source: Source::Mark,
                pid: None,
                payload: Payload::Mark {
                    label: format!("e-{i}"),
                },
            },
        )
        .unwrap();
        let _ack: DaemonToClient = read_frame(&mut emit_stream).unwrap().unwrap();
    }
    drop(emit_stream);

    let mut marks_seen = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while marks_seen < 3 && std::time::Instant::now() < deadline {
        let msg: Option<DaemonToClient> = read_frame(&mut sub_stream).unwrap_or(None);
        if let Some(DaemonToClient::EventNotification { event }) = msg {
            if let Payload::Mark { ref label } = event.payload {
                if label.starts_with("e-") {
                    marks_seen += 1;
                }
            }
        }
    }
    assert_eq!(marks_seen, 3, "subscriber missed mark events");

    drop(sub_stream);

    // Stop daemon.
    let mut stop_stream = UnixStream::connect(&sock).unwrap();
    write_frame(
        &mut stop_stream,
        &ClientToDaemon::Hello {
            client: "stop".into(),
        },
    )
    .unwrap();
    let _: DaemonToClient = read_frame(&mut stop_stream).unwrap().unwrap();
    write_frame(&mut stop_stream, &ClientToDaemon::Shutdown).unwrap();
    let _: DaemonToClient = read_frame(&mut stop_stream).unwrap().unwrap();
    drop(stop_stream);

    for _ in 0..50 {
        if daemon.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
}

//! #143: a scoped session must not outlive the record client that opened
//! it. If the connection that sent AttachScopedProbes dies without a
//! matching DetachScopedProbes (kill -9 on `smeltr record`), the daemon
//! auto-detaches: the session is finalized on disk and stops routing.

use smeltr_core::codec::{read_frame, write_frame};
use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};
use std::process::Stdio;
use std::time::Duration;

fn spawn_daemon(home: &std::path::Path, sock: &std::path::Path) -> std::process::Child {
    // smeltrd is a bin in this crate: Cargo builds it before this integration
    // test and exposes it via CARGO_BIN_EXE_smeltrd. (Do NOT shell out to
    // `cargo build` here — a nested cargo deadlocks on the outer build lock.)
    let daemon = std::process::Command::new(env!("CARGO_BIN_EXE_smeltrd"))
        .env("SMELTR_HOME", home)
        .env("SMELTR_SOCKET", sock)
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
    daemon
}

fn scoped_session_metadata(home: &std::path::Path) -> String {
    let sessions = home.join("sessions");
    let mut dirs: Vec<_> = std::fs::read_dir(&sessions)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    dirs.sort();
    for dir in dirs {
        let meta = std::fs::read_to_string(dir.join("metadata.toml")).unwrap_or_default();
        if meta.contains("type = \"Scoped\"") {
            return meta;
        }
    }
    panic!("no scoped session found under {}", sessions.display());
}

#[test]
#[serial_test::serial]
fn client_death_without_detach_finalizes_scoped_session() {
    let home = tempfile::tempdir().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let sock = sock_dir.path().join("sm.sock");
    let mut daemon = spawn_daemon(home.path(), &sock);

    {
        let mut s = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        write_frame(
            &mut s,
            &ClientToDaemon::AttachScopedProbes {
                pid: 999_999,
                argv: vec!["sleep".into(), "30".into()],
                scope_token: Some("orphan-test-token".into()),
                name: None,
                chunked: false,
            },
        )
        .unwrap();
        let resp: DaemonToClient = read_frame(&mut s).unwrap().unwrap();
        assert!(matches!(resp, DaemonToClient::Ack), "got {resp:?}");
        // Dropped here without DetachScopedProbes — simulates kill -9 on
        // the record client.
    }

    // The daemon must notice the disconnect and finalize the session.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let meta = loop {
        let meta = scoped_session_metadata(home.path());
        if meta.contains("ended_rfc3339") || std::time::Instant::now() > deadline {
            break meta;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        meta.contains("ended_rfc3339"),
        "scoped session was not finalized after client death:\n{meta}"
    );

    terminate(&mut daemon);
}

#[test]
#[serial_test::serial]
fn clean_detach_still_finalizes_with_exit_code() {
    let home = tempfile::tempdir().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let sock = sock_dir.path().join("sm.sock");
    let mut daemon = spawn_daemon(home.path(), &sock);

    let mut s = std::os::unix::net::UnixStream::connect(&sock).unwrap();
    write_frame(
        &mut s,
        &ClientToDaemon::AttachScopedProbes {
            pid: 999_998,
            argv: vec!["true".into()],
            scope_token: None,
            name: None,
            chunked: false,
        },
    )
    .unwrap();
    let _: DaemonToClient = read_frame(&mut s).unwrap().unwrap();
    write_frame(
        &mut s,
        &ClientToDaemon::DetachScopedProbes {
            pid: 999_998,
            exit_code: Some(0),
        },
    )
    .unwrap();
    let _: DaemonToClient = read_frame(&mut s).unwrap().unwrap();
    drop(s);

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let meta = loop {
        let meta = scoped_session_metadata(home.path());
        if meta.contains("exit_code") || std::time::Instant::now() > deadline {
            break meta;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        meta.contains("exit_code = 0"),
        "clean detach lost its exit code:\n{meta}"
    );

    terminate(&mut daemon);
}

/// SIGTERM + bounded wait, then force-kill: lets the daemon reap its
/// `log stream` child (#158) instead of orphaning it on every test run.
fn terminate(daemon: &mut std::process::Child) {
    let _ = std::process::Command::new("kill")
        .args(["-TERM", &daemon.id().to_string()])
        .output();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match daemon.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => break,
        }
    }
    let _ = daemon.kill();
    let _ = daemon.wait();
}

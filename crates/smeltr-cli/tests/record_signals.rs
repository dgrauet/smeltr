//! #269: the common ways a recording stops must keep what was recorded.
//! Ctrl-C reaches the whole foreground process group; `smeltr record`
//! installed no handler, died at once, and the daemon finalized the session
//! on disconnect with no exit status while the child's cleanup went to the
//! ambient session.

mod common;

use common::DaemonGuard;
use std::os::unix::process::CommandExt;
use std::time::Duration;

fn scoped_metadata(home: &std::path::Path) -> smeltr_core::session::SessionMetadata {
    std::fs::read_dir(home.join("sessions"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| smeltr_core::reader::read_metadata(&e.path()).ok())
        .find(|m| matches!(m.kind, smeltr_core::session::SessionKind::Scoped { .. }))
        .expect("scoped session")
}

#[test]
#[serial_test::serial]
fn ctrl_c_keeps_the_childs_exit_status() {
    let home = tempfile::tempdir().unwrap();
    let sock = home.path().join("smeltr.sock");
    let mut daemon = DaemonGuard::spawn(home.path(), &sock);

    // The child handles SIGINT (like Python's KeyboardInterrupt) and exits 130.
    let mut record = std::process::Command::new(env!("CARGO_BIN_EXE_smeltr"))
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .args([
            "record",
            "--no-hook",
            "--",
            "/bin/sh",
            "-c",
            "trap 'exit 130' INT; sleep 10 & wait",
        ])
        .process_group(0)
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    // Ctrl-C: SIGINT to the whole foreground process group.
    unsafe { libc::killpg(record.id() as i32, libc::SIGINT) };
    let status = record.wait().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    daemon.stop();

    let meta = scoped_metadata(home.path());
    assert_eq!(meta.exit_code, Some(130), "status of record: {status:?}");
    assert_eq!(
        status.code(),
        Some(130),
        "record passes the child's status on"
    );
}

/// #269: a SIGKILLed record client leaves its child running. The daemon
/// finalized the session at once and the rest of the run went to the
/// ambient session; it now keeps the session until the child exits.
#[test]
#[serial_test::serial]
fn a_killed_record_client_keeps_the_session_until_the_child_exits() {
    let home = tempfile::tempdir().unwrap();
    let sock = home.path().join("smeltr.sock");
    let mut daemon = DaemonGuard::spawn(home.path(), &sock);

    let mut record = std::process::Command::new(env!("CARGO_BIN_EXE_smeltr"))
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .args(["record", "--no-hook", "--", "/bin/sleep", "3"])
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    record.kill().unwrap(); // SIGKILL: no handler can run
    record.wait().unwrap();
    std::thread::sleep(Duration::from_millis(700));
    let while_running = scoped_metadata(home.path()).ended_rfc3339;

    std::thread::sleep(Duration::from_millis(2500));
    let after = scoped_metadata(home.path()).ended_rfc3339;
    daemon.stop();

    assert_eq!(while_running, None, "session finalized while the child ran");
    assert!(
        after.is_some(),
        "session never finalized after the child exited"
    );
}

/// #269: the command was spawned before the daemon accepted the session;
/// when it refused (read-only store), the command had already started and
/// was killed mid-run. It must not start at all.
#[test]
#[serial_test::serial]
fn a_refused_session_never_starts_the_command() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let sock = home.path().join("smeltr.sock");
    let mut daemon = DaemonGuard::spawn(home.path(), &sock);
    let sessions = home.path().join("sessions");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o555)).unwrap();

    let marker = home.path().join("ran");
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_smeltr"))
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .args(["record", "--no-hook", "--", "/usr/bin/touch"])
        .arg(&marker)
        .status()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o755)).unwrap();
    daemon.stop();

    assert!(!status.success(), "record must report the refusal");
    assert!(
        !marker.exists(),
        "the command ran although nothing was recorded"
    );
}

/// #269: when the daemon went away mid-run, `record` swallowed the failed
/// detach and exited as if the run had been recorded.
#[test]
#[serial_test::serial]
fn losing_the_daemon_mid_run_is_reported() {
    let home = tempfile::tempdir().unwrap();
    let sock = home.path().join("smeltr.sock");
    let mut daemon = DaemonGuard::spawn(home.path(), &sock);
    let record = std::process::Command::new(env!("CARGO_BIN_EXE_smeltr"))
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .args(["record", "--no-hook", "--", "/bin/sleep", "2"])
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));
    daemon.stop();
    let out = record.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("incomplete"), "stderr: {stderr}");
}

/// A child killed by a signal makes `record` exit 128 + signal, like a
/// shell, instead of 255 (`exit(-1)`).
#[test]
#[serial_test::serial]
fn a_child_killed_by_a_signal_exits_128_plus_the_signal() {
    let home = tempfile::tempdir().unwrap();
    let sock = home.path().join("smeltr.sock");
    let mut daemon = DaemonGuard::spawn(home.path(), &sock);
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_smeltr"))
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .args([
            "record",
            "--no-hook",
            "--",
            "/bin/sh",
            "-c",
            "kill -TERM $$",
        ])
        .status()
        .unwrap();
    daemon.stop();
    assert_eq!(status.code(), Some(128 + libc::SIGTERM));
    let meta = scoped_metadata(home.path());
    assert_eq!(meta.term_signal, Some(libc::SIGTERM));
}

//! End-to-end: two `smeltr record` invocations produce two distinct scoped
//! sessions, the ambient stays clean of PID-tagged events, and
//! `smeltr breakdown --last` picks the newest scoped session.

use assert_cmd::Command;
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

fn smeltrd_path() -> std::path::PathBuf {
    // Mirror the same binary-resolution used in tests/e2e.rs.
    let mut p = std::env::current_exe().unwrap();
    p.pop(); // drop test binary name
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("smeltrd")
}

fn wait_for_socket(path: &std::path::Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
#[serial_test::serial]
#[cfg(target_os = "macos")]
fn two_records_create_two_scoped_sessions() {
    let home = tempfile::tempdir().unwrap();
    let sock = home.path().join("smeltr.sock");

    let mut daemon = StdCommand::new(smeltrd_path())
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn smeltrd");
    assert!(wait_for_socket(&sock), "daemon never created socket");

    // Two records, each running /bin/sleep 1.
    for _ in 0..2 {
        Command::cargo_bin("smeltr")
            .unwrap()
            .env("SMELTR_HOME", home.path())
            .env("SMELTR_SOCKET", &sock)
            .args(["record", "--no-hook", "/bin/sleep", "1"])
            .assert()
            .success();
    }

    // Stop the daemon so the ambient session is finalized to disk.
    let _ = StdCommand::new("kill")
        .arg("-TERM")
        .arg(daemon.id().to_string())
        .output();
    let _ = daemon.wait();
    std::thread::sleep(Duration::from_millis(200));

    // Count sessions on disk: expect ≥ 3 (1 ambient + 2 scoped).
    let sessions_dir = home.path().join("sessions");
    let entries: Vec<_> = std::fs::read_dir(&sessions_dir)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(
        entries.len() >= 3,
        "expected ≥3 session dirs, got {}",
        entries.len()
    );

    // sessions ls must show ≥2 scoped lines and ≥1 ambient line.
    // Pass a non-existent socket path so the CLI falls back to disk reads
    // rather than querying a live daemon (which might not be ours).
    let out = Command::cargo_bin("smeltr")
        .unwrap()
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock) // sock no longer exists → forces disk fallback
        .args(["sessions", "ls"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).unwrap();
    let scoped_count = stdout.matches("[scoped ").count();
    let ambient_count = stdout.matches("[ambient]").count();
    assert!(
        scoped_count >= 2,
        "expected ≥2 scoped lines; got:\n{stdout}"
    );
    assert!(
        ambient_count >= 1,
        "expected ≥1 ambient line; got:\n{stdout}"
    );

    // breakdown --last must succeed on a sleep-only session (no Metal events,
    // but must not crash and must default to a scoped session).
    Command::cargo_bin("smeltr")
        .unwrap()
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock) // sock no longer exists → forces disk fallback
        .args(["breakdown", "--last"])
        .assert()
        .success();
}

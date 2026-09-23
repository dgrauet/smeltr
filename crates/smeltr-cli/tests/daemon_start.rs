//! `smeltr daemon start` must only report success for a daemon that serves
//! its socket, and must surface why it did not (#236).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// SIGTERMs whatever daemon the pid file names on drop, so a failing
/// assertion cannot leak a detached smeltrd (and its probes).
struct PidFileGuard(PathBuf);

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let Some(pid) = std::fs::read_to_string(&self.0)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        else {
            return;
        };
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && unsafe { libc::kill(pid, 0) } == 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

fn daemon_start(home: &Path, sock: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_smeltr"))
        .args(["daemon", "start"])
        .env("SMELTR_HOME", home)
        .env("SMELTR_SOCKET", sock)
        .output()
        .unwrap()
}

#[test]
fn start_fails_and_reports_why_when_the_daemon_dies_before_serving() {
    let home = tempfile::tempdir().unwrap();
    let _guard = PidFileGuard(home.path().join("smeltrd.pid"));
    // Longer than SUN_LEN (104): smeltrd claims its pid file, then fails to bind.
    let sock = home.path().join(format!("{}.sock", "x".repeat(120)));

    let out = daemon_start(home.path(), &sock);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "start must fail; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        stderr.contains("SUN_LEN"),
        "start must surface the daemon's own error; stderr: {stderr}"
    );
}

#[test]
fn start_succeeds_once_the_socket_accepts_connections() {
    let home = tempfile::tempdir().unwrap();
    let _guard = PidFileGuard(home.path().join("smeltrd.pid"));
    let sock = home.path().join("s.sock");

    let out = daemon_start(home.path(), &sock);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Success means served, not merely spawned.
    std::os::unix::net::UnixStream::connect(&sock).expect("socket must accept right after start");
}

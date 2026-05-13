//! Black-box end-to-end test: spawn the daemon as a child, run CLI commands,
//! assert their output.

use assert_cmd::Command;
use std::process::{Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

fn smeltrd_path() -> std::path::PathBuf {
    // assert_cmd places binaries in CARGO_TARGET_DIR or target/debug.
    let mut p = std::env::current_exe().unwrap();
    p.pop(); // drop test name
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("smeltrd")
}

fn wait_for_socket(path: &std::path::Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn end_to_end_mark_then_show() {
    let home = tempfile::tempdir().unwrap();
    let sock = home.path().join("smeltr.sock");

    let mut child = StdCommand::new(smeltrd_path())
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn smeltrd");
    assert!(wait_for_socket(&sock), "daemon never created its socket");

    Command::cargo_bin("smeltr")
        .unwrap()
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .args(["mark", "hello"])
        .assert()
        .success();

    Command::cargo_bin("smeltr")
        .unwrap()
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .args(["mark", "world"])
        .assert()
        .success();

    let out = Command::cargo_bin("smeltr")
        .unwrap()
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .args(["sessions", "ls"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listing = String::from_utf8(out).unwrap();
    let line = listing.lines().next().expect("at least one session listed");
    let short = line.rsplit('-').next().unwrap();

    // Shut down the daemon so it flushes events to disk.
    let _ = StdCommand::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .output();
    let _ = child.wait();
    std::thread::sleep(std::time::Duration::from_millis(100));

    let out = Command::cargo_bin("smeltr")
        .unwrap()
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .args(["sessions", "show", short])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let shown = String::from_utf8(out).unwrap();
    assert!(shown.contains("hello"), "stdout was:\n{shown}");
    assert!(shown.contains("world"), "stdout was:\n{shown}");
    assert!(shown.contains("session-started"), "stdout was:\n{shown}");
}

#[test]
fn doctor_prints_probe_status() {
    let out = Command::cargo_bin("smeltr")
        .unwrap()
        .arg("doctor")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8_lossy(&out);
    for probe in [
        "vm",
        "proc",
        "thermal",
        "oslog",
        "ioreport",
        "crash-reports",
        "mach-exceptions",
    ] {
        assert!(s.contains(probe), "doctor output missing {probe}:\n{s}");
    }
}

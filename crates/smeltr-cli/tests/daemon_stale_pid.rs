//! A pid file left by an unclean stop may name a reused pid: `smeltr daemon`
//! must not take that process for smeltrd (#242).

use std::process::Command;

fn daemon(home: &std::path::Path, cmd: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_smeltr"))
        .args(["daemon", cmd])
        .env("SMELTR_HOME", home)
        .env("SMELTR_SOCKET", home.join("s.sock"))
        .output()
        .unwrap()
}

#[test]
fn a_reused_pid_is_neither_reported_nor_signalled_as_smeltrd() {
    let home = tempfile::tempdir().unwrap();
    // Our own process, alive and not a smeltrd: what a reused pid looks like.
    let mut stranger = Command::new("sleep").arg("30").spawn().unwrap();
    let pid = stranger.id();
    std::fs::write(home.path().join("smeltrd.pid"), pid.to_string()).unwrap();

    let status = daemon(home.path(), "status");
    let stop = daemon(home.path(), "stop");
    let survived = stranger.try_wait().unwrap().is_none();
    let _ = stranger.kill();
    let _ = stranger.wait();

    let out = String::from_utf8_lossy(&status.stdout);
    assert!(
        out.contains(&format!("pid:    {pid} (stale")),
        "status: {out}"
    );
    assert!(
        stop.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(
        survived,
        "`daemon stop` signalled a process that is not smeltrd"
    );
}

/// The daemon side of the same trap: smeltrd refused to start ("another
/// smeltrd is already running") against a reused pid, and under launchd's
/// KeepAlive that is a relaunch loop.
#[test]
fn start_succeeds_over_a_pid_file_naming_another_process() {
    let home = tempfile::tempdir().unwrap();
    let pid_file = home.path().join("smeltrd.pid");
    let mut stranger = Command::new("sleep").arg("30").spawn().unwrap();
    std::fs::write(&pid_file, stranger.id().to_string()).unwrap();

    let start = daemon(home.path(), "start");
    // Whatever smeltrd now owns the pid file is ours to stop.
    let daemon_pid = std::fs::read_to_string(&pid_file)
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .filter(|p| *p != stranger.id() as i32);
    if let Some(p) = daemon_pid {
        unsafe { libc::kill(p, libc::SIGTERM) };
    }
    let _ = stranger.kill();
    let _ = stranger.wait();

    assert!(
        start.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(
        daemon_pid.is_some(),
        "smeltrd must have claimed the pid file"
    );
}

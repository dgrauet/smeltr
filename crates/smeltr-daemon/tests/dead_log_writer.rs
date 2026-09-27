//! A daemon whose log destination fails must keep serving. On a full disk
//! (launchd sends stdout to `smeltrd.log`, stderr to `smeltrd.err`), a
//! failed log write made
//! tracing-subscriber report it with `eprintln!`, which panics on a failed
//! stderr: the black-box hook then aborted smeltrd on every log line and
//! launchd relaunched it in a loop (14 aborts in 45 min on 2026-09-27).

use smeltr_core::codec::{read_frame, write_frame};
use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use std::time::Duration;

fn hello(sock: &std::path::Path) -> Option<DaemonToClient> {
    let mut s = UnixStream::connect(sock).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write_frame(
        &mut s,
        &ClientToDaemon::Hello {
            client: "dead-log-test".into(),
            scope_token: None,
        },
    )
    .ok()?;
    read_frame(&mut s).ok()?
}

/// A tiny HFS+ volume, detached on drop.
struct TinyVolume {
    mount: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

impl TinyVolume {
    fn new() -> Option<Self> {
        let dir = tempfile::tempdir().ok()?;
        let dmg = dir.path().join("v.dmg");
        let mount = dir.path().join("mnt");
        std::fs::create_dir(&mount).ok()?;
        let ok = |c: &mut std::process::Command| {
            c.stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        if !ok(std::process::Command::new("hdiutil")
            .args([
                "create",
                "-size",
                "4m",
                "-fs",
                "HFS+",
                "-volname",
                "smeltrlog",
            ])
            .arg(&dmg))
            || !ok(std::process::Command::new("hdiutil")
                .args(["attach", "-nobrowse", "-mountpoint"])
                .arg(&mount)
                .arg(&dmg))
        {
            return None;
        }
        Some(Self { mount, _dir: dir })
    }
}

impl Drop for TinyVolume {
    fn drop(&mut self) {
        let _ = std::process::Command::new("hdiutil")
            .args(["detach", "-force"])
            .arg(&self.mount)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[test]
#[serial_test::serial]
fn a_full_disk_under_the_log_file_does_not_kill_the_daemon() {
    let Some(vol) = TinyVolume::new() else {
        eprintln!("hdiutil unavailable; soft-skipping");
        return;
    };
    let log = std::fs::File::create(vol.mount.join("smeltrd.err")).unwrap();
    // Fill the volume: every later log write fails with ENOSPC.
    {
        use std::io::Write;
        let mut filler = std::fs::File::create(vol.mount.join("filler")).unwrap();
        for chunk in [65536usize, 512, 1] {
            while filler.write_all(&vec![0u8; chunk]).is_ok() {}
        }
    }

    let home = tempfile::tempdir().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let sock = sock_dir.path().join("sm.sock");
    let mut daemon = std::process::Command::new(env!("CARGO_BIN_EXE_smeltrd"))
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .env("RUST_LOG", "info")
        .arg("--foreground")
        // As under launchd: stdout (the log) and stderr both on the full disk.
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap();
    for _ in 0..50 {
        if sock.exists() || daemon.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    for _ in 0..5 {
        let _ = hello(&sock); // each logs "client connected"
    }
    std::thread::sleep(Duration::from_millis(300));
    let alive = daemon.try_wait().unwrap().is_none();
    let answered = matches!(hello(&sock), Some(DaemonToClient::Welcome { .. }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert!(alive, "smeltrd died when its log writes failed");
    assert!(answered, "smeltrd stopped answering");
}

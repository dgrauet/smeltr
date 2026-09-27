//! #267: the daemon must keep serving through transient `accept()` errors.
//! `Server::run` did `accept?`, so one EMFILE (fd limit reached) returned
//! from the accept loop for good: the process stayed alive — so launchd's
//! KeepAlive never restarted it — with its socket file in place, refusing
//! every connection from then on.

use smeltr_core::codec::{read_frame, write_frame};
use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::time::Duration;

fn hello(sock: &std::path::Path) -> Result<DaemonToClient, String> {
    let mut s = UnixStream::connect(sock).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write_frame(
        &mut s,
        &ClientToDaemon::Hello {
            client: "listener-test".into(),
            scope_token: None,
        },
    )
    .map_err(|e| e.to_string())?;
    read_frame(&mut s)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "connection closed".to_string())
}

#[test]
#[serial_test::serial]
fn daemon_keeps_accepting_after_running_out_of_file_descriptors() {
    let home = tempfile::tempdir().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let sock = sock_dir.path().join("sm.sock");
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_smeltrd"));
    cmd.env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .env("RUST_LOG", "warn")
        .arg("--foreground")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A low fd limit so a burst of connections exhausts it quickly.
    unsafe {
        cmd.pre_exec(|| {
            let lim = libc::rlimit {
                rlim_cur: 96,
                rlim_max: 96,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut daemon = cmd.spawn().unwrap();
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        matches!(hello(&sock), Ok(DaemonToClient::Welcome { .. })),
        "daemon not serving"
    );

    // Exhaust the daemon's descriptors, then give them all back.
    let mut held = Vec::new();
    for _ in 0..200 {
        match UnixStream::connect(&sock) {
            Ok(s) => held.push(s),
            Err(_) => break,
        }
    }
    std::thread::sleep(Duration::from_millis(500));
    drop(held);
    std::thread::sleep(Duration::from_millis(500));

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let last = loop {
        let r = hello(&sock);
        if matches!(r, Ok(DaemonToClient::Welcome { .. })) || std::time::Instant::now() > deadline {
            break r;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _ = daemon.kill();
    let _ = daemon.wait();
    assert!(
        matches!(last, Ok(DaemonToClient::Welcome { .. })),
        "daemon stopped serving after fd exhaustion: {last:?}"
    );
}

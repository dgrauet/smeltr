//! End-to-end: daemon panics -> hook saves the black box and aborts;
//! next boot recovers the orphaned session.

use serial_test::serial;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;

fn post_mortem_dir(home: &Path) -> Option<std::path::PathBuf> {
    std::fs::read_dir(home.join("sessions"))
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("post-mortem-daemon-panic-"))
        })
}

fn count_recovered(home: &Path) -> usize {
    std::fs::read_dir(home.join("sessions"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            smeltr_core::reader::read_metadata(&e.path())
                .map(|m| m.end_reason.as_deref() == Some("recovered-after-crash"))
                .unwrap_or(false)
        })
        .count()
}

/// Regression for #111: with a dead stderr (EPIPE on every write), the hook
/// must still write the post-mortem session + panic report. Before the fix,
/// the hook's own `eprintln!` double-panicked and aborted before the flush.
#[test]
#[serial]
fn panic_with_dead_stderr_still_saves_black_box() {
    let home = tempfile::tempdir().unwrap();
    let sock = home.path().join("smeltrd.sock");
    let bin = env!("CARGO_BIN_EXE_smeltrd");

    let mut child = std::process::Command::new(bin)
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .env("SMELTR_TEST_PANIC_MS", "300")
        // Silence tracing so the only stderr writers are the panic hook and
        // the default hook — the exact path #111 broke.
        .env("RUST_LOG", "off")
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Drop the read end: every subsequent stderr write in the child fails
    // with EPIPE (SIGPIPE is ignored in Rust processes).
    drop(child.stderr.take());
    let status = child.wait().unwrap();
    assert!(!status.success());

    let pm = post_mortem_dir(home.path())
        .expect("post-mortem session must be written even when stderr is dead");
    let report = std::fs::read_to_string(pm.join("panic-report.txt")).unwrap();
    assert!(report.contains("SMELTR_TEST_PANIC_MS fired"));
}

#[test]
#[serial]
fn panic_aborts_saves_black_box_and_next_boot_recovers() {
    let home = tempfile::tempdir().unwrap();
    let sock = home.path().join("smeltrd.sock");
    let bin = env!("CARGO_BIN_EXE_smeltrd");

    // 1. Daemon panics 300 ms after start -> hook must abort the process.
    let status = std::process::Command::new(bin)
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .env("SMELTR_TEST_PANIC_MS", "300")
        .status()
        .unwrap();
    assert!(!status.success());
    assert_eq!(status.signal(), Some(libc::SIGABRT), "hook must abort");

    // 2. Black box on disk: post-mortem session + panic report.
    let pm = post_mortem_dir(home.path()).expect("post-mortem session dir");
    let report = std::fs::read_to_string(pm.join("panic-report.txt")).unwrap();
    assert!(report.contains("SMELTR_TEST_PANIC_MS fired"));

    // 3. Ambient session was left unfinalized by the abort.
    assert_eq!(count_recovered(home.path()), 0);

    // 4. Restart -> boot recovery closes it; SIGTERM stops the daemon.
    let mut child = std::process::Command::new(bin)
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1000));
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let _ = child.wait();
    assert!(
        count_recovered(home.path()) >= 1,
        "orphaned session must be recovered"
    );
}

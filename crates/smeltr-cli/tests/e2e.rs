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
fn record_captures_child_lifecycle() {
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
        .args(["record", "/bin/sleep", "1"])
        .assert()
        .success();

    // Shut down the daemon so it flushes events to disk.
    let _ = StdCommand::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .output();
    let _ = child.wait();
    std::thread::sleep(Duration::from_millis(100));

    let sessions_root = home.path().join("sessions");
    let entries: Vec<_> = std::fs::read_dir(&sessions_root)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 1, "expected exactly one session directory");
    let events = smeltr_core::reader::read_events(&entries[0].path()).unwrap();
    let saw_exit = events.iter().any(|ev| match &ev.payload {
        smeltr_core::event::Payload::Mark { label } => label.contains("record:exit"),
        _ => false,
    });
    assert!(saw_exit, "expected record:exit marker in session events");
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore)]
fn record_with_metal_hook_captures_cb_lifecycle() {
    let dylib_rel = std::path::PathBuf::from("metal-hook/build/libmetal_hook.dylib");
    let candidates = [
        dylib_rel.clone(),
        std::path::PathBuf::from("../").join(&dylib_rel),
        std::path::PathBuf::from("../../").join(&dylib_rel),
    ];
    let dylib = candidates.iter().find(|p| p.exists()).cloned();
    let Some(dylib) = dylib else {
        eprintln!("metal-hook dylib not built — run `make -C metal-hook` first. Soft-skipping.");
        return;
    };
    let dylib_abs = std::fs::canonicalize(&dylib).unwrap();

    // Locate the harness binary BEFORE spawning the daemon, so an early
    // soft-skip doesn't leak a child process. assert_cmd 2.2.2 only resolves
    // CARGO_BIN_EXE_<name> for binaries in the same package; the harness
    // lives in a sibling crate, so we walk up from the running test binary
    // to the workspace target dir and locate it.
    let harness = {
        let exe = std::env::current_exe().expect("test exe path");
        let mut dir = exe.parent().expect("test exe parent").to_path_buf();
        while dir.file_name().is_some_and(|n| n != "target") {
            if !dir.pop() {
                panic!("could not find workspace target/ above {}", exe.display());
            }
        }
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        let candidate = dir.join(profile).join("smeltr-metal-harness");
        if !candidate.exists() {
            eprintln!(
                "smeltr-metal-harness binary not found at {} — soft-skipping. \
                 Run `cargo build -p smeltr-metal-harness` first.",
                candidate.display()
            );
            return;
        }
        candidate
    };

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().to_path_buf();
    let sock = tmp.path().join("smeltr.sock");

    let mut daemon = StdCommand::new(smeltrd_path())
        .env("SMELTR_HOME", &home)
        .env("SMELTR_SOCKET", &sock)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn smeltrd");
    assert!(wait_for_socket(&sock), "daemon never created its socket");

    Command::cargo_bin("smeltr")
        .unwrap()
        .env("SMELTR_HOME", &home)
        .env("SMELTR_SOCKET", &sock)
        .env("SMELTR_DYLIB", &dylib_abs)
        .args(["record", harness.to_str().unwrap()])
        .assert()
        .success();

    // Shut down the daemon so it flushes events to disk.
    let _ = StdCommand::new("kill")
        .arg("-TERM")
        .arg(daemon.id().to_string())
        .output();
    let _ = daemon.wait();
    std::thread::sleep(Duration::from_millis(200));

    let sessions_root = home.join("sessions");
    let entries: Vec<_> = std::fs::read_dir(&sessions_root)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 1);

    let dir = entries[0].path();
    let events = smeltr_core::reader::read_events(&dir).unwrap();

    use smeltr_core::event::Payload;
    let mut seen_committed = false;
    let mut seen_completed = false;
    let mut seen_buffer = false;
    for ev in &events {
        match &ev.payload {
            Payload::MetalCbCommitted { .. } => seen_committed = true,
            Payload::MetalCbCompleted { .. } => seen_completed = true,
            Payload::MetalBufferAlloc { .. } => seen_buffer = true,
            _ => {}
        }
    }
    assert!(
        seen_committed,
        "no MetalCbCommitted in session ({} events)",
        events.len()
    );
    assert!(seen_completed, "no MetalCbCompleted in session");
    assert!(seen_buffer, "no MetalBufferAlloc in session");
}

#[test]
#[serial_test::serial]
fn analyze_prints_report_from_session_dir() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("SMELTR_HOME", home.path());

    let fixture = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../smeltr-analyzer/fixtures/synthetic-watchdog.json"
    ))
    .unwrap();
    let events: Vec<smeltr_core::event::Event> = serde_json::from_str(&fixture).unwrap();

    // Write a session with these events.
    let id = smeltr_core::session::SessionId::new();
    let meta = smeltr_core::session::SessionMetadata::now_starting(id);
    let mut w = smeltr_core::writer::SessionWriter::create(meta).unwrap();
    for ev in &events {
        w.write_event(ev).unwrap();
    }
    let _dir = w.dir().to_path_buf();
    w.finalize(Some(0), "2026-05-14T00:00:00Z".into()).unwrap();

    let bin = env!("CARGO_BIN_EXE_smeltr");
    let out = std::process::Command::new(bin)
        .env("SMELTR_HOME", home.path())
        .args(["analyze", "--last"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "exit={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("ImpactingInteractivity"),
        "stdout was:\n{}",
        stdout
    );
    assert!(stdout.contains("Queue depth peaked"));
    assert!(stdout.contains("ReportCrash"));
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

#[test]
#[serial_test::serial]
fn tui_help_lists_subcommand() {
    let bin = env!("CARGO_BIN_EXE_smeltr");
    let out = std::process::Command::new(bin)
        .args(["tui", "--help"])
        .output()
        .unwrap();
    assert!(out.status.success(), "exit={:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Live TUI") || stdout.to_lowercase().contains("tui"));
}

#[test]
#[serial_test::serial]
fn sessions_open_help_lists_speed_flag() {
    let bin = env!("CARGO_BIN_EXE_smeltr");
    let out = std::process::Command::new(bin)
        .args(["sessions", "open", "--help"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "exit={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--speed"), "stdout was:\n{stdout}");
}

#[test]
#[serial_test::serial]
fn sessions_show_renders_metal_and_mlx_variants_readably() {
    use smeltr_core::event::{Event, Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    let home = tempfile::tempdir().unwrap();
    std::env::set_var("SMELTR_HOME", home.path());

    let id = SessionId::new();
    let meta = SessionMetadata::now_starting(id);
    let mut w = SessionWriter::create(meta).unwrap();
    w.write_event(&Event {
        ts_mono_ns: 1,
        ts_wall_ns: 1,
        session_id: Uuid::nil(),
        source: Source::MetalHook,
        pid: None,
        seq: 1,
        payload: Payload::MetalCbCommitted {
            cb_id: 0x1438e0,
            queue_id: 1,
            queue_depth: 23,
            label: None,
        },
    })
    .unwrap();
    w.write_event(&Event {
        ts_mono_ns: 2,
        ts_wall_ns: 2,
        session_id: Uuid::nil(),
        source: Source::MetalHook,
        pid: None,
        seq: 2,
        payload: Payload::MetalCbCompleted {
            cb_id: 0x1438e0,
            queue_id: 1,
            status: 4,
            error_code: Some(14),
            error_domain: Some("IOGPU".into()),
            in_flight_ns: 9_000_000_000,
        },
    })
    .unwrap();
    w.write_event(&Event {
        ts_mono_ns: 3,
        ts_wall_ns: 3,
        session_id: Uuid::nil(),
        source: Source::PythonSidecar,
        pid: None,
        seq: 3,
        payload: Payload::MlxMemoryPoll {
            active_bytes: 14 * 1024 * 1024 * 1024,
            peak_bytes: 18 * 1024 * 1024 * 1024,
            cache_bytes: 2 * 1024 * 1024 * 1024,
        },
    })
    .unwrap();
    w.finalize(Some(0), "2026-05-14T00:00:00Z".into()).unwrap();

    let bin = env!("CARGO_BIN_EXE_smeltr");
    let out = std::process::Command::new(bin)
        .env("SMELTR_HOME", home.path())
        .args(["sessions", "show", &id.short()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "exit={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for marker in [
        "MetalCbCommitted",
        "MetalCbCompleted",
        "cb_id=0x1438e0",
        "queue_depth=23",
        "error_code=14",
        "MlxMemoryPoll",
        "active=",
    ] {
        assert!(stdout.contains(marker), "missing {marker:?} in:\n{stdout}");
    }
    assert!(!stdout.contains("Payload {"));
}

//! `--last` / `--include-ambient` must pick sessions by start time, and
//! `breakdown --last` must not prefer post-mortems (#241).

use assert_cmd::Command;
use smeltr_core::event::{Event, Payload, Source};
use smeltr_core::session::{SessionId, SessionKind, SessionMetadata};
use smeltr_core::writer::SessionWriter;
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn mark() -> Event {
    Event {
        ts_mono_ns: 10,
        ts_wall_ns: 10,
        session_id: Uuid::nil(),
        source: Source::PythonSidecar,
        pid: None,
        seq: 1,
        payload: Payload::Mark {
            label: "m".into(),
            fields: Default::default(),
        },
    }
}

fn session(started: &str, kind: SessionKind, events: &[Event]) -> PathBuf {
    let mut meta = SessionMetadata::now_starting(SessionId::new());
    meta.started_rfc3339 = started.into();
    meta.kind = kind;
    let mut w = SessionWriter::create(meta).unwrap();
    for e in events {
        w.write_event(e).unwrap();
    }
    let dir = w.dir().to_path_buf();
    w.finalize(Some(0), "x".into()).unwrap();
    dir
}

fn scoped() -> SessionKind {
    SessionKind::Scoped {
        pid: 1,
        argv: vec!["run".into()],
    }
}

/// An empty session renamed the way the daemon names post-mortems.
fn post_mortem(started: &str, label: &str) -> PathBuf {
    let dir = session(started, SessionKind::Ambient, &[]);
    let name = dir.file_name().unwrap().to_string_lossy().to_string();
    let pm = dir
        .parent()
        .unwrap()
        .join(format!("post-mortem-{label}-{name}"));
    std::fs::rename(&dir, &pm).unwrap();
    pm
}

fn breakdown(home: &Path, args: &[&str]) -> std::process::Output {
    Command::cargo_bin("smeltr")
        .unwrap()
        .arg("breakdown")
        .args(args)
        .env("SMELTR_HOME", home)
        .output()
        .unwrap()
}

#[test]
#[serial_test::serial]
fn breakdown_last_ignores_a_fresher_post_mortem() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("SMELTR_HOME", home.path());
    session("2026-07-01T10:00:00Z", scoped(), &[mark()]);
    post_mortem("2026-07-01T10:05:00Z", "crash-report");

    let out = breakdown(home.path(), &["--last"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !stdout.contains("no events captured"),
        "picked the empty post-mortem: {stdout}"
    );
}

#[test]
#[serial_test::serial]
fn include_ambient_picks_the_newest_session_by_start_time() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("SMELTR_HOME", home.path());
    // `post-mortem-*` sorts after every dated directory by name.
    post_mortem("2026-06-01T00:00:00Z", "crash-report");
    session("2026-07-01T10:00:00Z", scoped(), &[mark()]);

    let out = breakdown(home.path(), &["--last", "--include-ambient"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !stdout.contains("no events captured"),
        "picked the month-old post-mortem: {stdout}"
    );
}

#[test]
#[serial_test::serial]
fn last_and_a_session_argument_conflict() {
    let home = tempfile::tempdir().unwrap();
    for cmd in ["breakdown", "analyze"] {
        let out = Command::cargo_bin("smeltr")
            .unwrap()
            .args([cmd, "--last", "abc"])
            .env("SMELTR_HOME", home.path())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && stderr.contains("cannot be used with"),
            "{cmd}: --last plus an id must be rejected, got: {stderr}"
        );
    }
}

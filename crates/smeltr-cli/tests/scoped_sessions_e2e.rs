//! End-to-end: two `smeltr record` invocations produce two distinct scoped
//! sessions, the ambient stays clean of PID-tagged events, and
//! `smeltr breakdown --last` picks the newest scoped session.

mod common;

use assert_cmd::Command;
use common::DaemonGuard;
use std::time::Duration;

#[test]
#[serial_test::serial]
#[cfg(target_os = "macos")]
fn two_records_create_two_scoped_sessions() {
    let home = tempfile::tempdir().unwrap();
    let sock = home.path().join("smeltr.sock");

    let mut daemon = DaemonGuard::spawn(home.path(), &sock);

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
    daemon.stop();
    std::thread::sleep(Duration::from_millis(100));

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

/// A real `smeltr record` must produce ProcFootprint samples in its scoped
/// session: this is the only test covering routing end to end, from the probe
/// down to the session file.
#[test]
#[serial_test::serial]
#[cfg(target_os = "macos")]
fn record_emits_proc_footprint_in_scoped_session() {
    let home = tempfile::tempdir().unwrap();
    let sock = home.path().join("smeltr.sock");

    // `SMELTR_FOOTPRINT_PERIOD_MS` is read by `FootprintProbe::default_period()`
    // inside `ProbeRuntime::attach_scoped`, which runs in the *daemon*
    // process — not in the `smeltr record` client. Setting it on the record
    // command (as an earlier version of this test did) is a no-op: the
    // daemon was already spawned with its own environment by the time
    // `record` runs, so the probe silently falls back to the 2s default.
    // That earlier version of the test passed anyway, because it only
    // asserted `scoped_root_samples > 0`, which the 2s-default cadence also
    // satisfies over a 1s sleep — the assertion couldn't tell a working knob
    // from a no-op one. Set the variable on the daemon here instead, and
    // assert a count that only a ~200ms cadence can produce.
    let mut daemon =
        DaemonGuard::spawn_with_env(home.path(), &sock, &[("SMELTR_FOOTPRINT_PERIOD_MS", "200")]);

    Command::cargo_bin("smeltr")
        .unwrap()
        .env("SMELTR_HOME", home.path())
        .env("SMELTR_SOCKET", &sock)
        .args(["record", "--no-hook", "/bin/sleep", "2"])
        .assert()
        .success();

    daemon.stop();
    std::thread::sleep(Duration::from_millis(100));

    // Relire toutes les sessions et compter les ProcFootprint par type.
    let sessions_dir = home.path().join("sessions");
    let mut scoped_root_samples = 0usize;
    let mut ambient_samples = 0usize;
    for entry in std::fs::read_dir(&sessions_dir).unwrap().flatten() {
        let dir = entry.path();
        let Ok(meta) = smeltr_core::reader::read_metadata(&dir) else {
            continue;
        };
        let Ok(events) = smeltr_core::reader::read_events(&dir) else {
            continue;
        };
        let is_ambient = matches!(meta.kind, smeltr_core::session::SessionKind::Ambient);
        for e in &events {
            if let smeltr_core::event::Payload::ProcFootprint {
                is_traced_root,
                phys_footprint_bytes,
                ..
            } = &e.payload
            {
                assert!(
                    *phys_footprint_bytes > 0,
                    "une empreinte nulle n'a pas de sens"
                );
                if is_ambient {
                    ambient_samples += 1;
                } else if *is_traced_root {
                    scoped_root_samples += 1;
                }
            }
        }
    }

    // A 2s-default cadence over a 2s sleep would produce ~1 sample; a 200ms
    // cadence produces ~10. Require ≥5 so the assertion actually
    // distinguishes "the knob worked" from "the knob was ignored" — do not
    // weaken this back to `> 0`.
    assert!(
        scoped_root_samples >= 5,
        "expected >=5 root ProcFootprint samples (200ms cadence over ~2s), got \
         {scoped_root_samples} — the default cadence would yield only ~1"
    );
    assert_eq!(
        ambient_samples, 0,
        "the ambient session must receive no ProcFootprint at all"
    );
}

//! Boot-time crash recovery: sessions left with `ended_rfc3339 == None` by a
//! daemon that died without finalizing (panic-abort, SIGKILL, segfault) get
//! their metadata closed with `end_reason = "recovered-after-crash"`.
//!
//! The events payload is never rewritten: the chunked reader already
//! scan-recovers sealed chunks, and reopening a possibly-truncated zstd
//! stream would risk corrupting what survived.

use smeltr_core::reader::{list_sessions, read_metadata};
use smeltr_core::session::{events_path_for_read, write_metadata};
use std::path::Path;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

/// Returns Some(pid) when `pid_path` names a process that is still alive.
pub fn live_daemon_pid(pid_path: &Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pid_path)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    // kill(pid, 0) probes existence without signaling. EPERM means the
    // process exists but belongs to another user — still alive.
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return Some(pid);
    }
    (std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)).then_some(pid)
}

/// Atomically claims the pid file (`O_EXCL`). Fails with `AlreadyExists`
/// when another daemon won the race between the liveness check and this
/// call — the loser must bail without touching the winner's file.
pub fn claim_pid_file(pid_path: &Path) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(pid_path)?;
    f.write_all(std::process::id().to_string().as_bytes())
}

/// Marks every non-finalized, non-post-mortem session as recovered.
/// Returns how many sessions were recovered. Call ONLY when no other
/// daemon is alive.
pub fn recover_orphaned_sessions() -> std::io::Result<usize> {
    let mut recovered = 0;
    for dir in list_sessions()? {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with("post-mortem-") {
            continue;
        }
        let Ok(mut meta) = read_metadata(&dir) else {
            continue; // unreadable metadata: not ours to repair
        };
        if meta.ended_rfc3339.is_some() {
            continue;
        }
        // events-file mtime ≈ last write ≈ crash time; cheaper and more
        // robust than decoding a possibly-truncated stream.
        meta.ended_rfc3339 = Some(events_mtime_rfc3339(&dir).unwrap_or_else(now_rfc3339));
        meta.end_reason = Some("recovered-after-crash".to_string());
        if let Err(e) = write_metadata(&dir, &meta) {
            // One unwritable session must not abort the whole pass.
            tracing::warn!(dir = %dir.display(), error = %e, "failed to rewrite session metadata; skipping");
            continue;
        }
        recovered += 1;
    }
    Ok(recovered)
}

fn events_mtime_rfc3339(dir: &Path) -> Option<String> {
    let mtime = std::fs::metadata(events_path_for_read(dir))
        .ok()?
        .modified()
        .ok()?;
    OffsetDateTime::from(mtime).format(&Rfc3339).ok()
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use smeltr_core::event::{Payload, Source};
    use smeltr_core::reader::read_metadata;
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;

    fn temp_home() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", d.path());
        d
    }

    /// Creates an on-disk session; when `finalized` is false the writer is
    /// dropped without finalize (simulates a crash mid-recording).
    fn make_session(finalized: bool) -> std::path::PathBuf {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let dir = smeltr_core::session::session_dir(&meta);
        let mut w = SessionWriter::create(meta).unwrap();
        w.write_event(&smeltr_core::event::Event {
            ts_mono_ns: 1,
            ts_wall_ns: 1,
            session_id: uuid::Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: 1,
            payload: Payload::Mark {
                label: "x".into(),
                fields: Default::default(),
            },
        })
        .unwrap();
        if finalized {
            w.finalize(Some(0), "2026-07-13T00:00:00Z".into()).unwrap();
        } else {
            w.flush().unwrap();
            drop(w);
        }
        dir
    }

    #[test]
    #[serial]
    fn recovers_orphaned_session_and_leaves_finalized_alone() {
        let _h = temp_home();
        let orphan = make_session(false);
        let done = make_session(true);
        let n = recover_orphaned_sessions().unwrap();
        assert_eq!(n, 1);
        let m = read_metadata(&orphan).unwrap();
        assert!(m.ended_rfc3339.is_some());
        assert_eq!(m.end_reason.as_deref(), Some("recovered-after-crash"));
        let m2 = read_metadata(&done).unwrap();
        assert_eq!(m2.end_reason, None);
        assert_eq!(m2.exit_code, Some(0));
        // Idempotent: second run recovers nothing.
        assert_eq!(recover_orphaned_sessions().unwrap(), 0);
    }

    #[test]
    #[serial]
    fn skips_post_mortem_dirs() {
        let h = temp_home();
        let pm = h
            .path()
            .join("sessions")
            .join("post-mortem-daemon-panic-x-deadbeef");
        std::fs::create_dir_all(&pm).unwrap();
        assert_eq!(recover_orphaned_sessions().unwrap(), 0);
    }

    #[test]
    fn live_daemon_pid_treats_eperm_as_alive() {
        // kill(1, 0) targets launchd: EPERM as non-root, 0 as root — either
        // way the process exists, so it must read as alive.
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("smeltrd.pid");
        std::fs::write(&p, "1").unwrap();
        assert_eq!(live_daemon_pid(&p), Some(1));
    }

    #[test]
    #[serial]
    fn recovery_continues_past_unwritable_session_metadata() {
        use std::os::unix::fs::PermissionsExt;
        let h = temp_home();
        let root = h.path().join("sessions");
        // Two hand-built orphan sessions; "aaa-*" sorts first and its
        // metadata.toml is made read-only so write_metadata fails on it.
        for name in ["aaa-orphan", "bbb-orphan"] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let meta = SessionMetadata::now_starting(SessionId::new());
            smeltr_core::session::write_metadata(&dir, &meta).unwrap();
        }
        let locked = root.join("aaa-orphan").join("metadata.toml");
        let mut perms = std::fs::metadata(&locked).unwrap().permissions();
        perms.set_mode(0o444);
        std::fs::set_permissions(&locked, perms).unwrap();

        let n = recover_orphaned_sessions().unwrap();
        assert_eq!(n, 1, "the writable orphan must still be recovered");
        let m = read_metadata(&root.join("bbb-orphan")).unwrap();
        assert_eq!(m.end_reason.as_deref(), Some("recovered-after-crash"));
    }

    #[test]
    fn claim_pid_file_writes_our_pid_when_absent() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("smeltrd.pid");
        claim_pid_file(&p).unwrap();
        assert_eq!(
            std::fs::read_to_string(&p).unwrap().trim(),
            std::process::id().to_string()
        );
    }

    #[test]
    fn claim_pid_file_fails_when_already_present() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("smeltrd.pid");
        std::fs::write(&p, "12345").unwrap();
        let err = claim_pid_file(&p).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        // The loser must not clobber the winner's pid file.
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "12345");
    }

    #[test]
    fn live_daemon_pid_detects_dead_and_alive() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("smeltrd.pid");
        // Our own pid is alive.
        std::fs::write(&p, std::process::id().to_string()).unwrap();
        assert_eq!(live_daemon_pid(&p), Some(std::process::id()));
        // A certainly-dead pid (pid_max on macOS is 99998; 4_000_000 never exists).
        std::fs::write(&p, "4000000").unwrap();
        assert_eq!(live_daemon_pid(&p), None);
        // Missing / garbage files.
        std::fs::write(&p, "not-a-pid").unwrap();
        assert_eq!(live_daemon_pid(&p), None);
        std::fs::remove_file(&p).unwrap();
        assert_eq!(live_daemon_pid(&p), None);
    }
}

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
    // kill(pid, 0) probes existence without signaling.
    (unsafe { libc::kill(pid as i32, 0) } == 0).then_some(pid)
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
        write_metadata(&dir, &meta)?;
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

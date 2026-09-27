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

/// Executable name of the daemon, as the kernel reports it.
pub const DAEMON_EXE: &str = "smeltrd";

/// Returns Some(pid) when `pid_path` names a running smeltrd.
///
/// Checking only that the pid is alive (`kill(pid, 0)`) was not enough: the
/// pid file outlives an unclean stop — panic abort, `kill -9`, power loss —
/// and the pid is then reused, at boot by anything. smeltrd refused to start
/// against that stranger and launchd's KeepAlive relaunched it in a loop,
/// and `smeltr daemon stop` would have signalled it (#242).
pub fn live_daemon_pid(pid_path: &Path) -> Option<u32> {
    live_pid_named(pid_path, DAEMON_EXE)
}

/// Returns Some(pid) when `pid_path` names a live process whose executable
/// is called `exe`.
pub fn live_pid_named(pid_path: &Path, exe: &str) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pid_path)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    (process_exe_name(pid)? == exe).then_some(pid)
}

/// Basename of a live process's executable; `None` once it has exited.
/// `proc_pidpath` answers for other users' processes too (root included),
/// unlike `proc_pidinfo(PROC_PIDTBSDINFO)`.
fn process_exe_name(pid: u32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe {
        libc::proc_pidpath(
            i32::try_from(pid).ok()?,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
        )
    };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    let path = String::from_utf8(buf).ok()?;
    path.rsplit('/').next().map(str::to_string)
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
        // The periodic flush makes events durable every 500 ms, so the mtime
        // is within that of the last event. It can still predate the start
        // (clock step, copied session): never end before starting.
        let ended = events_mtime(&dir).unwrap_or_else(OffsetDateTime::now_utc);
        let ended = match OffsetDateTime::parse(&meta.started_rfc3339, &Rfc3339) {
            Ok(started) if ended < started => started,
            _ => ended,
        };
        meta.ended_rfc3339 = Some(ended.format(&Rfc3339).unwrap_or_else(|_| now_rfc3339()));
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

fn events_mtime(dir: &Path) -> Option<OffsetDateTime> {
    let mtime = std::fs::metadata(events_path_for_read(dir))
        .ok()?
        .modified()
        .ok()?;
    Some(OffsetDateTime::from(mtime))
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

/// Remove every file in `<SMELTR_HOME>/rings`. Called at boot, when no
/// recording is attached: a ring still there belongs to a recording whose
/// client died before removing it (#269). A hook still writing to one keeps
/// its mapping; the new daemon would not drain it anyway. Returns the count.
pub fn reap_orphan_rings() -> usize {
    let dir = smeltr_core::session::smeltr_home().join("rings");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return 0;
    };
    entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter(|e| std::fs::remove_file(e.path()).is_ok())
        .count()
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

    fn mark(seq: u64) -> smeltr_core::event::Event {
        smeltr_core::event::Event {
            ts_mono_ns: seq,
            ts_wall_ns: seq,
            session_id: uuid::Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq,
            payload: Payload::Mark {
                label: "x".into(),
                fields: Default::default(),
            },
        }
    }

    fn parse(s: &str) -> OffsetDateTime {
        OffsetDateTime::parse(s, &Rfc3339).unwrap()
    }

    /// #268: a chunked session killed after a short run must end where its
    /// last periodic flush landed. The flush used to write nothing under
    /// 4 KiB, so the events file kept its creation mtime and the recovered
    /// session ended the instant it started — and held no event.
    #[test]
    #[serial]
    fn recovered_chunked_session_ends_at_its_last_flush() {
        let _h = temp_home();
        let meta = SessionMetadata::now_starting(SessionId::new());
        let dir = smeltr_core::session::session_dir(&meta);
        let mut w = SessionWriter::create_with_format(meta, true).unwrap();
        w.write_event(&mark(1)).unwrap();
        w.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1200));
        w.write_event(&mark(2)).unwrap();
        w.flush().unwrap();
        drop(w); // daemon SIGKILL
        assert_eq!(recover_orphaned_sessions().unwrap(), 1);
        let m = read_metadata(&dir).unwrap();
        let (start, end) = (
            parse(&m.started_rfc3339),
            parse(m.ended_rfc3339.as_deref().unwrap()),
        );
        assert!(
            end - start >= time::Duration::milliseconds(1100),
            "ended {end} must follow the last flush, started {start}"
        );
        assert_eq!(smeltr_core::reader::read_events(&dir).unwrap().len(), 2);
    }

    /// The events file's mtime can predate the start (a clock step, a copied
    /// session): the recovered end never precedes the start.
    #[test]
    #[serial]
    fn recovered_end_never_precedes_start() {
        let _h = temp_home();
        let orphan = make_session(false);
        let f = std::fs::File::options()
            .write(true)
            .open(smeltr_core::session::events_path_for_read(&orphan))
            .unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000))
            .unwrap();
        drop(f);
        recover_orphaned_sessions().unwrap();
        let m = read_metadata(&orphan).unwrap();
        assert!(parse(m.ended_rfc3339.as_deref().unwrap()) >= parse(&m.started_rfc3339));
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
    fn a_process_that_is_not_smeltrd_is_not_a_live_daemon() {
        // #242: after an unclean stop (panic abort, kill -9, power loss) the
        // pid file survives and the pid gets reused — at boot, by anything.
        // pid 1 is launchd: alive, and not a smeltrd.
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("smeltrd.pid");
        std::fs::write(&p, "1").unwrap();
        assert_eq!(live_daemon_pid(&p), None);
        // Nor is this test binary, alive as it is.
        std::fs::write(&p, std::process::id().to_string()).unwrap();
        assert_eq!(live_daemon_pid(&p), None);
    }

    #[test]
    #[serial]
    fn recovery_continues_past_unwritable_session_metadata() {
        use std::os::unix::fs::PermissionsExt;
        let h = temp_home();
        let root = h.path().join("sessions");
        // Two hand-built orphan sessions; "aaa-*" sorts first and its
        // directory is made read-only so write_metadata fails on it (the
        // metadata is replaced by rename, so the file's own mode no longer
        // matters — the directory's does).
        for name in ["aaa-orphan", "bbb-orphan"] {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let meta = SessionMetadata::now_starting(SessionId::new());
            smeltr_core::session::write_metadata(&dir, &meta).unwrap();
        }
        let locked = root.join("aaa-orphan");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

        let n = recover_orphaned_sessions().unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
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
        // Our own pid is alive — under the name of our own executable.
        let me = std::env::current_exe().unwrap();
        let me = me.file_name().unwrap().to_str().unwrap();
        std::fs::write(&p, std::process::id().to_string()).unwrap();
        assert_eq!(live_pid_named(&p, me), Some(std::process::id()));
        // A certainly-dead pid (pid_max on macOS is 99998; 4_000_000 never exists).
        std::fs::write(&p, "4000000").unwrap();
        assert_eq!(live_pid_named(&p, me), None);
        // Missing / garbage files.
        std::fs::write(&p, "not-a-pid").unwrap();
        assert_eq!(live_pid_named(&p, me), None);
        std::fs::remove_file(&p).unwrap();
        assert_eq!(live_pid_named(&p, me), None);
    }

    /// #269: rings of recordings whose client died were never removed (8
    /// orphans, 128 MB, some months old, in a real ~/.smeltr/rings). At boot
    /// no recording is attached, so every ring left there is an orphan.
    #[test]
    #[serial_test::serial]
    fn boot_removes_leftover_rings() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let rings = home.path().join("rings");
        std::fs::create_dir_all(&rings).unwrap();
        for name in ["a.ring", "a.ring.123", "b.ring.9.tmp"] {
            std::fs::write(rings.join(name), b"x").unwrap();
        }
        std::fs::write(home.path().join("keep.txt"), b"x").unwrap();
        assert_eq!(reap_orphan_rings(), 3);
        assert_eq!(std::fs::read_dir(&rings).unwrap().count(), 0);
        assert!(home.path().join("keep.txt").exists());
    }
}

//! Black-box panic flush: on any daemon panic, best-effort save the flight
//! recorder to a post-mortem session (with a panic-report.txt), best-effort
//! flush all sessions, then abort the process (fail-fast: a panicked task
//! must never leave a silently degraded daemon behind).
//!
//! Everything here runs on the panicking thread: only `try_lock` is used
//! (a held mutex would deadlock), poisoned locks are recovered, and every
//! step is isolated so one failure cannot prevent the next.

use crate::flight_recorder::FlightRecorder;
use crate::session_router::SessionRouter;
use crate::triggers::{self, TriggerReason};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Weak;

static PANICKING: AtomicBool = AtomicBool::new(false);

pub fn install_panic_hook(router: Weak<SessionRouter>, fr: Weak<FlightRecorder>) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // A panic inside the hook itself must not recurse.
        if PANICKING.swap(true, Ordering::SeqCst) {
            std::process::abort();
        }
        let message = match info.location() {
            Some(loc) => format!(
                "{} at {}:{}",
                payload_message(info.payload()),
                loc.file(),
                loc.line()
            ),
            None => payload_message(info.payload()),
        };
        let backtrace = std::backtrace::Backtrace::force_capture().to_string();
        // Disk first, console last: a panic anywhere in the hook aborts the
        // process before the flush (#111 — eprintln! double-panicked on a
        // dead stderr under ENOSPC and the black box was lost). The default
        // hook only writes to stderr, so it runs after the flush too.
        handle_panic(&message, &backtrace, &router, &fr, &mut std::io::stderr());
        default_hook(info); // default message + location to stderr
        std::process::abort();
    }));
}

/// Everything the hook does except the final abort (kept separate for tests).
///
/// Must never panic: writes to `out` are best-effort (stderr can be dead —
/// EPIPE under launchd, ENOSPC on full disk) and all disk writes happen
/// before any console output.
fn handle_panic(
    message: &str,
    backtrace: &str,
    router: &Weak<SessionRouter>,
    fr: &Weak<FlightRecorder>,
    out: &mut dyn Write,
) {
    let mut post_mortem_note = String::new();
    if let Some(fr) = fr.upgrade() {
        let events = fr.try_snapshot().unwrap_or_default();
        let reason = TriggerReason::DaemonPanic {
            message: message.to_string(),
        };
        match triggers::flush_post_mortem_events(events, &reason) {
            Ok(summary) => {
                if let Err(e) = write_panic_report(&summary.session_dir, message, backtrace) {
                    post_mortem_note = format!("panic-report write failed: {e}");
                } else {
                    post_mortem_note = format!(
                        "black box saved to {} ({} events)",
                        summary.session_dir.display(),
                        summary.event_count
                    );
                }
            }
            Err(e) => post_mortem_note = format!("post-mortem write failed: {e}"),
        }
    }
    let flushed = match router.upgrade() {
        Some(router) => router.try_flush_all(),
        None => 0,
    };
    // Console output only after every disk write is done.
    let _ = writeln!(out, "smeltrd panic: {message}");
    if !post_mortem_note.is_empty() {
        let _ = writeln!(out, "smeltrd panic: {post_mortem_note}");
    }
    let _ = writeln!(out, "smeltrd panic: flushed {flushed} session(s)");
}

fn payload_message(payload: &dyn std::any::Any) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

fn write_panic_report(
    dir: &std::path::Path,
    message: &str,
    backtrace: &str,
) -> std::io::Result<()> {
    std::fs::write(
        dir.join("panic-report.txt"),
        format!("smeltrd panic\n\nmessage: {message}\n\nbacktrace:\n{backtrace}\n"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flight_recorder::FlightRecorder;
    use crate::session_router::SessionRouter;
    use crate::sessions::ActiveSession;
    use serial_test::serial;
    use std::sync::Arc;

    #[test]
    fn payload_message_extracts_str_and_string() {
        assert_eq!(payload_message(&"boom"), "boom");
        assert_eq!(payload_message(&String::from("boom2")), "boom2");
        assert_eq!(payload_message(&42_u32), "non-string panic payload");
    }

    #[test]
    fn write_panic_report_contains_message_and_backtrace() {
        let dir = tempfile::tempdir().unwrap();
        write_panic_report(dir.path(), "boom at src/x.rs:1", "bt-line-1").unwrap();
        let text = std::fs::read_to_string(dir.path().join("panic-report.txt")).unwrap();
        assert!(text.contains("boom at src/x.rs:1"));
        assert!(text.contains("bt-line-1"));
    }

    /// Writer that fails every write, like a dead stderr (EPIPE/ENOSPC).
    struct FailingWriter;
    impl std::io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe))
        }
    }

    #[test]
    #[serial]
    fn handle_panic_writes_post_mortem_and_flushes() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let fr = Arc::new(FlightRecorder::new(std::time::Duration::from_secs(60)));
        let ambient = Arc::new(ActiveSession::open_new_full(Some(fr.clone()), None).unwrap());
        let router = Arc::new(SessionRouter::new(ambient.clone(), Some(fr.clone()), None));
        // #111 regression: a failing console writer must not panic nor
        // prevent the post-mortem flush.
        handle_panic(
            "boom",
            "bt",
            &Arc::downgrade(&router),
            &Arc::downgrade(&fr),
            &mut FailingWriter,
        );
        let sessions_root = home.path().join("sessions");
        let pm = std::fs::read_dir(&sessions_root)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("post-mortem-daemon-panic-")
            })
            .expect("post-mortem session dir");
        assert!(pm.path().join("panic-report.txt").exists());
        ambient.finalize(Some(0), "test").unwrap();
        std::env::remove_var("SMELTR_HOME");
    }
}

//! `get_crash_report` tool: return the crash report behind a session.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_core::event::Payload;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    pub session: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub crash_report_path: Option<String>,
    pub text: Option<String>,
    pub size_bytes: Option<u64>,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let dir = resolve_session(&params.session)?;
    let Some(path) = report_path(&dir) else {
        return Ok(Response {
            crash_report_path: None,
            text: None,
            size_bytes: None,
        });
    };
    // The report may have been deleted since; say where it was anyway.
    let text = std::fs::read_to_string(&path).ok();
    let size_bytes = std::fs::metadata(&path).ok().map(|m| m.len());
    Ok(Response {
        crash_report_path: Some(path),
        text,
        size_bytes,
    })
}

/// Where the session's crash report lives. Sessions never hold a copy (a
/// `crash-reports/` directory used to be read here that nothing writes, so
/// the tool always came back empty — #242):
/// - a post-mortem (or ambient) session carries the `CrashReportEmitted`
///   event; the newest one is the report that triggered the flush;
/// - a recorded run that crashed ended before ReportCrash wrote its report,
///   so it is joined from DiagnosticReports exactly as `analyze` does.
///
/// A post-mortem records its trigger (#267): its snapshot holds every crash
/// on the machine in the last 60 s, so the newest one was any app's.
fn report_path(dir: &std::path::Path) -> Option<String> {
    let trigger = smeltr_core::reader::read_metadata(dir)
        .ok()
        .and_then(|m| m.post_mortem);
    if let Some(t) = &trigger {
        if let Some(path) = &t.crash_report {
            return Some(path.clone());
        }
    }
    let events = smeltr_core::reader::read_events(dir).unwrap_or_default();
    if let Some(t) = trigger {
        // Only a crash of the process the post-mortem is about.
        let pid = t.pid?;
        return events.iter().rev().find_map(|e| match &e.payload {
            Payload::CrashReportEmitted {
                path,
                crashed_pid: Some(p),
                ..
            } if *p == pid => Some(path.clone()),
            _ => None,
        });
    }
    let emitted = events.iter().rev().find_map(|e| match &e.payload {
        Payload::CrashReportEmitted { path, .. } => Some(path.clone()),
        _ => None,
    });
    emitted.or_else(|| smeltr_analyzer::crash_join::session_crash(dir).map(|j| j.path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Event, Payload, Source};
    use smeltr_core::session::{SessionId, SessionKind, SessionMetadata};
    use smeltr_core::writer::SessionWriter;

    const MULTILINE: &str =
        include_str!("../../../smeltr-probes-crash-reports/tests/fixtures/sample_multiline.ips");

    fn crash_event(path: &std::path::Path, seq: u64) -> Event {
        Event {
            ts_mono_ns: seq,
            ts_wall_ns: seq,
            session_id: uuid::Uuid::nil(),
            source: Source::CrashReport,
            pid: None,
            seq,
            payload: Payload::CrashReportEmitted {
                path: path.display().to_string(),
                crashed_pid: Some(1),
                signal: None,
                exception_codes: vec![],
                summary: String::new(),
                proc_name: None,
            },
        }
    }

    #[test]
    #[serial_test::serial]
    fn returns_none_for_a_session_without_crash() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());
        let id = SessionId::new();
        drop(SessionWriter::create(SessionMetadata::now_starting(id)).unwrap());
        let resp = run(Params {
            session: id.short(),
        });
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");
        assert!(resp.unwrap().text.is_none());
    }

    /// A post-mortem holds the report that triggered it — the newest
    /// `CrashReportEmitted` of its flight-recorder snapshot (#242).
    #[test]
    #[serial_test::serial]
    fn returns_the_report_a_post_mortem_was_written_for() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        let older = reports.path().join("older.ips");
        let trigger = reports.path().join("trigger.ips");
        std::fs::write(&older, "older report").unwrap();
        std::fs::write(&trigger, "trigger report").unwrap();

        let id = SessionId::new();
        let mut w = SessionWriter::create(SessionMetadata::now_starting(id)).unwrap();
        w.write_event(&crash_event(&older, 1)).unwrap();
        w.write_event(&crash_event(&trigger, 2)).unwrap();
        w.finalize(None, "post-mortem".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
        })
        .unwrap();
        assert_eq!(resp.text.as_deref(), Some("trigger report"));
        assert_eq!(
            resp.crash_report_path.as_deref(),
            Some(trigger.to_str().unwrap())
        );
        assert_eq!(resp.size_bytes, Some(14));
    }

    /// #267: the snapshot holds every crash on the machine in the last 60 s,
    /// so "newest" was any app's. A post-mortem now records its trigger.
    #[test]
    #[serial_test::serial]
    fn a_post_mortem_returns_its_trigger_not_the_newest_crash() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        let trigger = reports.path().join("trigger.ips");
        let later = reports.path().join("unrelated.ips");
        std::fs::write(&trigger, "trigger report").unwrap();
        std::fs::write(&later, "unrelated report").unwrap();

        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.post_mortem = Some(smeltr_core::session::PostMortemTrigger {
            reason: "crash-report".into(),
            crash_report: Some(trigger.display().to_string()),
            pid: Some(1),
        });
        let mut w = SessionWriter::create(meta).unwrap();
        w.write_event(&crash_event(&trigger, 1)).unwrap();
        w.write_event(&crash_event(&later, 2)).unwrap();
        w.finalize(None, "post-mortem".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
        })
        .unwrap();
        assert_eq!(resp.text.as_deref(), Some("trigger report"));
    }

    /// A post-mortem for a Metal error has no crash report of its own; an
    /// unrelated crash in its snapshot is not one.
    #[test]
    #[serial_test::serial]
    fn a_metal_error_post_mortem_has_no_crash_report() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());
        let unrelated = reports.path().join("unrelated.ips");
        std::fs::write(&unrelated, "unrelated report").unwrap();

        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.post_mortem = Some(smeltr_core::session::PostMortemTrigger {
            reason: "metal-error".into(),
            crash_report: None,
            pid: None,
        });
        let mut w = SessionWriter::create(meta).unwrap();
        w.write_event(&crash_event(&unrelated, 1)).unwrap();
        w.finalize(None, "post-mortem".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
        });
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");
        assert!(resp.unwrap().crash_report_path.is_none());
    }

    /// A recorded run that crashed: ReportCrash writes the `.ips` after the
    /// session ends, so it is joined from DiagnosticReports like `analyze`
    /// does (#153) — the session directory never held a copy (#242).
    #[test]
    #[serial_test::serial]
    fn joins_the_report_of_a_crashed_recording() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());
        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        // The fixture's crashed pid.
        meta.kind = SessionKind::Scoped {
            pid: 11672,
            argv: vec!["python".into()],
        };
        drop(SessionWriter::create(meta).unwrap());
        let ips = reports.path().join("python-2026-07-16.ips");
        std::fs::write(&ips, MULTILINE).unwrap();

        let resp = run(Params {
            session: id.short(),
        });
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");
        let resp = resp.unwrap();
        // The session keeps its own copy (#270: macOS purges
        // DiagnosticReports), which is what the tool now returns.
        let path = resp.crash_report_path.as_deref().unwrap();
        assert!(path.ends_with("crash-report.ips"), "{path}");
        assert!(!path.starts_with(reports.path().to_str().unwrap()));
        assert_eq!(resp.text.as_deref(), Some(MULTILINE));
    }
}

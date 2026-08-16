//! `get_session_summary` tool: run the analyzer on a session.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_analyzer::Report;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    pub session: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub report: Report,
    pub event_count: usize,
    /// Path of the captured `.gputrace`, when `smeltr record --gputrace` asked
    /// for one. Openable in Xcode's Metal debugger, which goes down to
    /// per-encoder and per-shader detail — where `export_session` stops at the
    /// op level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gputrace_path: Option<String>,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let dir = resolve_session(&params.session)?;
    let events = smeltr_core::reader::read_events(&dir)?;
    let mut report = smeltr_analyzer::analyze(&events);
    // Same joins as `smeltr analyze`: a crash verdict must surface through
    // both, otherwise the MCP layer substitutes the memory-death presumption
    // for a perfectly real crash (#204).
    smeltr_analyzer::crash_join::join_crash(&mut report, &dir);
    smeltr_analyzer::crash_join::join_jetsam(&mut report, &dir);
    let gputrace_path = smeltr_core::reader::read_metadata(&dir)
        .ok()
        .and_then(|m| m.gputrace_path);
    Ok(Response {
        report,
        event_count: events.len(),
        gputrace_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Event, Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    #[test]
    #[serial_test::serial]
    fn summarizes_session_with_root_cause() {
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
            payload: Payload::MetalCbCompleted {
                cb_id: 1,
                queue_id: 1,
                status: 4,
                error_code: Some(14),
                error_domain: Some("IOGPU".into()),
                in_flight_ns: 1,
            },
        })
        .unwrap();
        w.finalize(Some(0), "2026-05-14T00:00:00Z".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
        })
        .unwrap();
        assert!(resp.event_count >= 1);
        assert!(resp.report.root_cause().is_some());
    }

    #[test]
    #[serial_test::serial]
    fn unknown_session_returns_not_found() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let r = run(Params {
            session: "nope".into(),
        });
        assert!(matches!(r, Err(ToolError::NotFound(_))));
    }

    /// #204: the MCP layer must surface a crash verdict, like `smeltr analyze`
    /// does. The join used to live in the CLI only, so the MCP said nothing
    /// about the crash — and since #201 it substituted the memory-death
    /// presumption, saying something inaccurate being worse than staying
    /// silent.
    #[test]
    #[serial_test::serial]
    fn crash_verdict_reaches_the_mcp_surface() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.kind = smeltr_core::session::SessionKind::Scoped {
            pid: 11672,
            argv: vec!["/usr/bin/python3".into()],
        };
        let mut w = SessionWriter::create(meta).unwrap();
        // An event stamped now bounds the window on the present, hence on the
        // mtime of the report written just below.
        w.write_event(&Event {
            ts_mono_ns: 1,
            ts_wall_ns: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64,
            session_id: Uuid::nil(),
            source: Source::Proc,
            pid: Some(11672),
            seq: 1,
            payload: Payload::ProcFootprint {
                pid: 11672,
                name: "python3".into(),
                phys_footprint_bytes: 1_000_000,
                lifetime_max_phys_footprint_bytes: 1_000_000,
                is_traced_root: true,
            },
        })
        .unwrap();
        w.finalize(Some(-1), "x".into()).unwrap();

        let fixture = include_str!(
            "../../../smeltr-probes-crash-reports/tests/fixtures/sample_multiline.ips"
        );
        std::fs::write(reports.path().join("Python-crash.ips"), fixture).unwrap();

        let resp = run(Params {
            session: id.short(),
        })
        .unwrap();
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        let root = resp
            .report
            .findings
            .iter()
            .find(|f| f.category == smeltr_analyzer::Category::RootCause)
            .expect("le verdict de crash doit sortir par le MCP");
        assert!(root.title.contains("crashed"), "title: {}", root.title);
    }
}

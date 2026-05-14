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
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let dir = resolve_session(&params.session)?;
    let events = smeltr_core::reader::read_events(&dir)?;
    let report = smeltr_analyzer::analyze(&events);
    Ok(Response {
        report,
        event_count: events.len(),
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
}

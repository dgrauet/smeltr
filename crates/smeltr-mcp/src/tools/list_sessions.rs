//! `list_sessions` tool: enumerate sessions on disk.

use crate::types::ToolError;
use serde::{Deserialize, Serialize};
use smeltr_core::reader::{list_sessions, read_events, read_metadata};

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct Params {}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub sessions: Vec<SessionSummary>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub short_id: String,
    pub full_id: String,
    pub dir_name: String,
    pub started_rfc3339: String,
    pub ended_rfc3339: Option<String>,
    pub exit_code: Option<i32>,
    pub event_count: usize,
    pub root_cause_title: Option<String>,
}

pub fn run(_params: Params) -> Result<Response, ToolError> {
    let dirs = list_sessions()?;
    let mut out = Vec::with_capacity(dirs.len());
    for dir in dirs.iter() {
        let dir_name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let meta = read_metadata(dir).ok();
        let events = read_events(dir).unwrap_or_default();
        let report = smeltr_analyzer::analyze(&events);
        let root_cause_title = report.root_cause().map(|f| f.title.clone());
        let (full_id, started, ended, exit_code) = match meta {
            Some(m) => (
                m.session_id.to_string(),
                m.started_rfc3339.clone(),
                m.ended_rfc3339.clone(),
                m.exit_code,
            ),
            None => (String::new(), String::new(), None, None),
        };
        let short_id = if full_id.len() >= 8 {
            full_id[..8].to_string()
        } else {
            full_id.clone()
        };
        out.push(SessionSummary {
            short_id,
            full_id,
            dir_name,
            started_rfc3339: started,
            ended_rfc3339: ended,
            exit_code,
            event_count: events.len(),
            root_cause_title,
        });
    }
    Ok(Response { sessions: out })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Event, Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    fn make_session(events: &[Event]) {
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        for e in events {
            w.write_event(e).unwrap();
        }
        w.finalize(Some(0), "2026-05-14T00:00:00Z".into()).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn empty_home_returns_empty_list() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let resp = run(Params::default()).unwrap();
        assert!(resp.sessions.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn lists_one_session_with_root_cause_from_analyzer() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        make_session(&[Event {
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
        }]);
        let resp = run(Params::default()).unwrap();
        assert_eq!(resp.sessions.len(), 1);
        let s = &resp.sessions[0];
        assert_eq!(s.event_count, 1);
        assert!(s
            .root_cause_title
            .as_ref()
            .unwrap()
            .contains("ImpactingInteractivity"));
    }
}

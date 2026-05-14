//! `find_correlations` tool: events from other sources within ± window of focal.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_core::event::Event;

const DEFAULT_WINDOW_NS: u64 = 200_000_000;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    pub session: String,
    pub focal_seq: u64,
    pub window_ns: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub focal: Event,
    pub window_ns: u64,
    pub correlated: Vec<Event>,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let dir = resolve_session(&params.session)?;
    let events = smeltr_core::reader::read_events(&dir)?;
    let focal = events
        .iter()
        .find(|e| e.seq == params.focal_seq)
        .cloned()
        .ok_or_else(|| {
            ToolError::NotFound(format!("focal seq {} not in session", params.focal_seq))
        })?;
    let window = params.window_ns.unwrap_or(DEFAULT_WINDOW_NS);
    let from = focal.ts_mono_ns.saturating_sub(window);
    let to = focal.ts_mono_ns.saturating_add(window);
    let correlated: Vec<Event> = events
        .into_iter()
        .filter(|e| e.seq != focal.seq)
        .filter(|e| e.source != focal.source)
        .filter(|e| e.ts_mono_ns >= from && e.ts_mono_ns <= to)
        .collect();
    Ok(Response {
        focal,
        window_ns: window,
        correlated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    #[test]
    #[serial_test::serial]
    fn finds_events_from_other_sources_in_window() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        // Focal: MetalHook event at 1s, seq=42.
        w.write_event(&Event {
            ts_mono_ns: 1_000_000_000,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq: 42,
            payload: Payload::MetalCbCompleted {
                cb_id: 1,
                queue_id: 1,
                status: 4,
                error_code: Some(14),
                error_domain: None,
                in_flight_ns: 1,
            },
        })
        .unwrap();
        // Within window: Mark at 1.05s.
        w.write_event(&Event {
            ts_mono_ns: 1_050_000_000,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: 43,
            payload: Payload::Mark {
                label: "within".into(),
            },
        })
        .unwrap();
        // Out of window: Mark at 2s.
        w.write_event(&Event {
            ts_mono_ns: 2_000_000_000,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: 44,
            payload: Payload::Mark {
                label: "outside".into(),
            },
        })
        .unwrap();
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
            focal_seq: 42,
            window_ns: None,
        })
        .unwrap();
        assert_eq!(resp.correlated.len(), 1);
        assert_eq!(resp.correlated[0].seq, 43);
    }

    #[test]
    #[serial_test::serial]
    fn unknown_focal_seq_is_not_found() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let w = SessionWriter::create(meta).unwrap();
        drop(w);
        let r = run(Params {
            session: id.short(),
            focal_seq: 999,
            window_ns: None,
        });
        assert!(matches!(r, Err(ToolError::NotFound(_))));
    }
}

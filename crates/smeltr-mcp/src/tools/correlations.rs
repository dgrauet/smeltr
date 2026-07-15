//! `find_correlations` tool: events from other sources within ± window of
//! focal — capped and relevance-ranked (#132: the unranked full window
//! returned 3.1 MB on a dense real session and blew past MCP client
//! limits). Notable events (errors, marks, sampling-state changes, model
//! loads…) come first, then routine telemetry by temporal proximity; what
//! is dropped is summarized per kind in `elided`.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_core::event::{Event, Payload};
use std::collections::BTreeMap;

const DEFAULT_WINDOW_NS: u64 = 200_000_000;
const DEFAULT_MAX_EVENTS: usize = 50;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    pub session: String,
    pub focal_seq: u64,
    pub window_ns: Option<u64>,
    /// Cap on returned correlated events (default 50).
    pub max_events: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub focal: Event,
    pub window_ns: u64,
    pub correlated: Vec<Event>,
    /// Events in the window that were dropped by the cap, counted per
    /// payload kind.
    pub elided: BTreeMap<String, u64>,
}

/// High-frequency telemetry that is almost never the story by itself.
/// Anything NOT in this list (errors, marks, crashes, model loads,
/// sampling-state changes…) ranks first.
const ROUTINE_KINDS: &[&str] = &[
    "MetalBufferAlloc",
    "MetalBufferFree",
    "MetalHeapAlloc",
    "MetalHeapFree",
    "MetalTextureAlloc",
    "MetalTextureFree",
    "MetalCbCommitted",
    "MetalCbScheduled",
    "MetalCbCompleted",
    "MetalCbOps",
    "MetalDeviceMemSample",
    "MlxMemoryPoll",
    "MlxEvalEntered",
    "MlxEvalReturned",
    "ModuleEntered",
    "ModuleReturned",
    "VmSample",
    "ProcTop",
    "ThermalState",
    "IoReportSample",
];

fn payload_kind(p: &Payload) -> String {
    serde_json::to_value(p)
        .ok()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
        .unwrap_or_else(|| "Unknown".to_string())
}

fn is_notable(e: &Event) -> bool {
    // A failed CB is notable even though CbCompleted is routine.
    if let Payload::MetalCbCompleted { error_code, .. } = &e.payload {
        return error_code.is_some_and(|c| c != 0);
    }
    !ROUTINE_KINDS.contains(&payload_kind(&e.payload).as_str())
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
    let max_events = params.max_events.unwrap_or(DEFAULT_MAX_EVENTS);
    let from = focal.ts_mono_ns.saturating_sub(window);
    let to = focal.ts_mono_ns.saturating_add(window);
    let mut in_window: Vec<Event> = events
        .into_iter()
        .filter(|e| e.seq != focal.seq)
        .filter(|e| e.source != focal.source)
        .filter(|e| e.ts_mono_ns >= from && e.ts_mono_ns <= to)
        .collect();
    // Notable first, then by distance to the focal timestamp.
    in_window.sort_by_key(|e| (!is_notable(e), e.ts_mono_ns.abs_diff(focal.ts_mono_ns)));
    let mut elided: BTreeMap<String, u64> = BTreeMap::new();
    if in_window.len() > max_events {
        for e in &in_window[max_events..] {
            *elided.entry(payload_kind(&e.payload)).or_default() += 1;
        }
        in_window.truncate(max_events);
    }
    Ok(Response {
        focal,
        window_ns: window,
        correlated: in_window,
        elided,
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
                fields: Default::default(),
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
                fields: Default::default(),
            },
        })
        .unwrap();
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
            focal_seq: 42,
            window_ns: None,
            max_events: None,
        })
        .unwrap();
        assert_eq!(resp.correlated.len(), 1);
        assert!(resp.elided.is_empty());
        assert_eq!(resp.correlated[0].seq, 43);
    }

    /// #132: dense sessions returned megabytes. The response is capped,
    /// notable events (a Mark here) outrank routine telemetry even when
    /// farther from the focal, and the drop is summarized per kind.
    #[test]
    #[serial_test::serial]
    fn caps_ranks_and_reports_elided() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        w.write_event(&Event {
            ts_mono_ns: 1_000_000_000,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq: 1,
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
        // 200 routine allocs, closest to the focal.
        for i in 0..200u64 {
            w.write_event(&Event {
                ts_mono_ns: 1_000_000_100 + i,
                ts_wall_ns: 0,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 100 + i,
                payload: Payload::MlxMemoryPoll {
                    active_bytes: 1,
                    cache_bytes: 1,
                    peak_bytes: 1,
                },
            })
            .unwrap();
        }
        // One Mark near the edge of the window: must rank FIRST anyway.
        w.write_event(&Event {
            ts_mono_ns: 1_150_000_000,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: 999,
            payload: Payload::Mark {
                label: "vae-decode-start".into(),
                fields: Default::default(),
            },
        })
        .unwrap();
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
            focal_seq: 1,
            window_ns: None,
            max_events: None,
        })
        .unwrap();
        assert_eq!(resp.correlated.len(), 50, "capped at the default");
        assert_eq!(
            resp.correlated[0].seq, 999,
            "the Mark outranks closer routine telemetry"
        );
        assert_eq!(resp.elided.get("MlxMemoryPoll").copied(), Some(151));
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
            max_events: None,
        });
        assert!(matches!(r, Err(ToolError::NotFound(_))));
    }
}

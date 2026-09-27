//! `find_correlations` tool: events from other sources within ± window of
//! focal — capped and relevance-ranked (#132: the unranked full window
//! returned 3.1 MB on a dense real session and blew past MCP client
//! limits). Notable events (errors, marks, sampling-state changes, model
//! loads…) come first, then routine telemetry by temporal proximity; what
//! is dropped is summarized per kind in `elided`.

use crate::types::{bounded_count, resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_core::event::{Event, Payload};
use smeltr_core::filter::payload_kind;
use std::collections::BTreeMap;
use std::ops::ControlFlow;

const DEFAULT_WINDOW_NS: u64 = 200_000_000;
const DEFAULT_MAX_EVENTS: usize = 50;
/// `max_events: 100000` returned 4.3 M characters (#271).
const MAX_MAX_EVENTS: usize = 500;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    pub session: String,
    pub focal_seq: u64,
    pub window_ns: Option<u64>,
    /// Cap on returned correlated events (default 50, 1..=500).
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

fn is_notable(e: &Event) -> bool {
    // A failed CB is notable even though CbCompleted is routine.
    if let Payload::MetalCbCompleted { error_code, .. } = &e.payload {
        return error_code.is_some_and(|c| c != 0);
    }
    !ROUTINE_KINDS.contains(&payload_kind(e))
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let max_events = bounded_count(
        "max_events",
        params.max_events,
        DEFAULT_MAX_EVENTS,
        MAX_MAX_EVENTS,
    )?;
    let dir = resolve_session(&params.session)?;
    // Two streaming passes rather than the whole session in memory (#271):
    // the focal event (stopping there), then the window around it, whose
    // time bounds let a chunked session skip every other chunk.
    let mut focal = None;
    crate::session_cache::visit(&dir, None, |e| {
        if e.seq == params.focal_seq {
            focal = Some(e.clone());
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })?;
    let focal = focal.ok_or_else(|| {
        ToolError::NotFound(format!("focal seq {} not in session", params.focal_seq))
    })?;
    let window = params.window_ns.unwrap_or(DEFAULT_WINDOW_NS);
    let filter = smeltr_core::EventFilter {
        from_ts: Some(focal.ts_mono_ns.saturating_sub(window)),
        to_ts: Some(focal.ts_mono_ns.saturating_add(window)),
        ..Default::default()
    };
    let mut in_window: Vec<Event> = Vec::new();
    crate::session_cache::visit(&dir, Some(&filter), |e| {
        if e.seq != focal.seq && e.source != focal.source {
            in_window.push(e.clone());
        }
        ControlFlow::Continue(())
    })?;
    // Notable first, then by distance to the focal timestamp.
    in_window.sort_by_key(|e| (!is_notable(e), e.ts_mono_ns.abs_diff(focal.ts_mono_ns)));
    let mut elided: BTreeMap<String, u64> = BTreeMap::new();
    if in_window.len() > max_events {
        for e in &in_window[max_events..] {
            *elided.entry(payload_kind(e).to_string()).or_default() += 1;
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

    /// #271: `max_events: 100000` returned 4.3 M characters.
    #[test]
    #[serial_test::serial]
    fn out_of_range_max_events_is_bad_args() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        for max_events in [0, 501, 100_000] {
            let r = run(Params {
                session: "deadbeef".into(),
                focal_seq: 1,
                window_ns: None,
                max_events: Some(max_events),
            });
            assert!(
                matches!(&r, Err(ToolError::BadArgs(m)) if m.contains("500")),
                "max_events {max_events}: {r:?}"
            );
        }
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

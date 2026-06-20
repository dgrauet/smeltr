//! `subscribe_live` tool: delta summary of a running session since a cursor.
//!
//! Poll-based "live tail": each call reads the session log from disk, slices
//! events after `cursor`, and returns a rolled-up delta. The cursor is an
//! event-sequence index (count consumed). See the design spec for why disk-tail
//! beats a live bus bridge for a turn-based LLM consumer.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_core::event::{Event, Payload};
use smeltr_core::reader::{list_sessions, read_metadata};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct Params {
    /// Session ref (short id / UUID / name). Omit to target the most-recent
    /// live session. Pass back the returned `session_id` on every later poll.
    pub session: Option<String>,
    /// Events already consumed. Omit/0 for a baseline summary from the start.
    pub cursor: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct PayloadCount {
    pub kind: String,
    pub count: u64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct GpuDelta {
    pub new_cbs: u64,
    pub gpu_ms_added: f64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct OpDelta {
    pub kind: String,
    pub count: u64,
    pub gpu_ms: f64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct MemDelta {
    pub current_bytes: u64,
    pub peak_bytes_session: u64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelLoadDelta {
    pub name: String,
    pub bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub session_id: String,
    pub session_name: Option<String>,
    pub live: bool,
    pub cursor: u64,
    pub prev_cursor: u64,
    pub new_events: u64,
    pub elapsed_ns: u64,
    pub by_payload: Vec<PayloadCount>,
    pub gpu: GpuDelta,
    pub top_ops: Vec<OpDelta>,
    pub memory: MemDelta,
    pub model_loads: Vec<ModelLoadDelta>,
    pub note: Option<String>,
}

pub fn summarize_delta(
    events: &[Event],
    cursor: u64,
    session_id: String,
    session_name: Option<String>,
    live: bool,
) -> Response {
    let len = events.len() as u64;
    let clamped = cursor > len;
    let cur = cursor.min(len) as usize;
    let delta = &events[cur..];
    let new_events = delta.len() as u64;

    let elapsed_ns = match (events.first(), events.last()) {
        (Some(a), Some(b)) => b.ts_mono_ns.saturating_sub(a.ts_mono_ns),
        _ => 0,
    };

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    for e in delta {
        *counts
            .entry(crate::tools::query_events::payload_kind(e))
            .or_insert(0) += 1;
    }
    let mut by_payload: Vec<PayloadCount> = counts
        .into_iter()
        .map(|(kind, count)| PayloadCount {
            kind: kind.to_string(),
            count,
        })
        .collect();
    by_payload.sort_by(|a, b| b.count.cmp(&a.count).then(a.kind.cmp(&b.kind)));

    // GPU: completed CBs + summed op GPU time over the delta window.
    let mut new_cbs: u64 = 0;
    let mut gpu_ns_total: u64 = 0;
    let mut op_acc: BTreeMap<String, (u64, u64)> = BTreeMap::new(); // kind -> (gpu_ns, count)
    let mut model_loads: Vec<ModelLoadDelta> = Vec::new();
    for e in delta {
        match &e.payload {
            Payload::MetalCbCompleted { .. } => new_cbs += 1,
            Payload::MetalCbOps { ops, .. } => {
                for o in ops {
                    gpu_ns_total += o.gpu_ns;
                    let kind = o
                        .symbol
                        .as_deref()
                        .and_then(smeltr_analyzer::resolve_kind)
                        .map(|k| k.to_string())
                        .unwrap_or_else(|| o.name.clone());
                    let slot = op_acc.entry(kind).or_insert((0, 0));
                    slot.0 += o.gpu_ns;
                    slot.1 += o.count as u64;
                }
            }
            Payload::ModelLoad {
                path, size_bytes, ..
            } => {
                let name = std::path::Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path.as_str())
                    .to_string();
                model_loads.push(ModelLoadDelta {
                    name,
                    bytes: *size_bytes,
                });
            }
            _ => {}
        }
    }
    let gpu = GpuDelta {
        new_cbs,
        gpu_ms_added: gpu_ns_total as f64 / 1e6,
    };
    let mut top_ops: Vec<OpDelta> = op_acc
        .into_iter()
        .map(|(kind, (gpu_ns, count))| OpDelta {
            kind,
            count,
            gpu_ms: gpu_ns as f64 / 1e6,
        })
        .collect();
    top_ops.sort_by(|a, b| {
        b.gpu_ms
            .partial_cmp(&a.gpu_ms)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.kind.cmp(&b.kind))
    });
    top_ops.truncate(8);

    // Memory: scan history up to the new cursor.
    let mut current_bytes: u64 = 0;
    let mut peak_bytes_session: u64 = 0;
    for e in &events[..cur] {
        if let Payload::MetalDeviceMemSample {
            allocated_bytes, ..
        } = &e.payload
        {
            current_bytes = *allocated_bytes;
            peak_bytes_session = peak_bytes_session.max(*allocated_bytes);
        }
    }
    let memory = MemDelta {
        current_bytes,
        peak_bytes_session,
    };

    let note = if clamped {
        Some("cursor ahead of log (clamped)".to_string())
    } else if new_events == 0 {
        Some(if live {
            "no new events".to_string()
        } else {
            "session finalized; no new events".to_string()
        })
    } else if !live {
        Some("session finalized".to_string())
    } else {
        None
    };

    Response {
        session_id,
        session_name,
        live,
        cursor: len,
        prev_cursor: cur as u64,
        new_events,
        elapsed_ns,
        by_payload,
        gpu,
        top_ops,
        memory,
        model_loads,
        note,
    }
}

fn is_live(dir: &std::path::Path) -> bool {
    read_metadata(dir)
        .map(|m| m.ended_rfc3339.is_none())
        .unwrap_or(false)
}

/// Pick the most-recent live session; fall back to the most-recent overall
/// (with `live=false`). Errors only when no session exists at all.
fn resolve_live_session() -> Result<(PathBuf, bool), ToolError> {
    let metas: Vec<(PathBuf, smeltr_core::session::SessionMetadata)> = list_sessions()?
        .into_iter()
        .filter_map(|d| read_metadata(&d).ok().map(|m| (d, m)))
        .collect();

    let newest = |pred: &dyn Fn(&smeltr_core::session::SessionMetadata) -> bool| {
        metas
            .iter()
            .filter(|(_, m)| pred(m))
            .max_by(|a, b| a.1.started_rfc3339.cmp(&b.1.started_rfc3339))
            .map(|(d, _)| d.clone())
    };

    if let Some(d) = newest(&|m| m.ended_rfc3339.is_none()) {
        return Ok((d, true));
    }
    if let Some(d) = newest(&|_| true) {
        return Ok((d, false));
    }
    Err(ToolError::BadArgs("no sessions found".to_string()))
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let (dir, live) = match params.session.as_deref() {
        Some(s) => {
            let dir = resolve_session(s)?;
            let live = is_live(&dir);
            (dir, live)
        }
        None => resolve_live_session()?,
    };
    let meta = read_metadata(&dir).ok();
    let session_id = meta
        .as_ref()
        .map(|m| m.session_id.to_string())
        .unwrap_or_default();
    let session_name = meta.as_ref().and_then(|m| m.name.clone());
    let events = smeltr_core::reader::read_events(&dir)?;
    Ok(summarize_delta(
        &events,
        params.cursor.unwrap_or(0),
        session_id,
        session_name,
        live,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Payload, Source};
    use uuid::Uuid;

    fn ev(seq: u64, source: Source, payload: Payload) -> Event {
        Event {
            ts_mono_ns: seq * 10,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source,
            pid: None,
            seq,
            payload,
        }
    }

    fn op(name: &str, symbol: &str, gpu_ns: u64, count: u32) -> smeltr_core::event::OpSample {
        smeltr_core::event::OpSample {
            name: name.into(),
            symbol: Some(symbol.into()),
            gpu_ns,
            count,
        }
    }

    #[test]
    fn gpu_and_top_ops_aggregate_over_delta() {
        let evs = vec![
            ev(
                0,
                Source::Mark,
                Payload::Mark {
                    label: "x".into(),
                    fields: Default::default(),
                },
            ),
            ev(
                1,
                Source::MetalHook,
                Payload::MetalCbCompleted {
                    cb_id: 1,
                    queue_id: 1,
                    status: 0,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 5_000_000,
                },
            ),
            ev(
                2,
                Source::MetalHook,
                Payload::MetalCbOps {
                    cb_id: 1,
                    ops: vec![
                        op("gemm", "gemm_t_n_bf16", 2_000_000, 3),
                        op("softmax", "softmax_kernel", 1_000_000, 1),
                    ],
                },
            ),
        ];
        // cursor=1 -> delta is the CbCompleted + CbOps (skip the Mark)
        let r = summarize_delta(&evs, 1, "sid".into(), None, true);
        assert_eq!(r.gpu.new_cbs, 1);
        assert!((r.gpu.gpu_ms_added - 3.0).abs() < 1e-9); // (2e6 + 1e6)/1e6
        assert_eq!(r.top_ops.len(), 2);
        assert_eq!(r.top_ops[0].kind, "Matmul"); // gemm_* -> Matmul, largest gpu_ns first
        assert!((r.top_ops[0].gpu_ms - 2.0).abs() < 1e-9);
        assert_eq!(r.top_ops[0].count, 3);
    }

    #[test]
    fn memory_uses_history_up_to_cursor() {
        let evs = vec![
            ev(
                0,
                Source::MetalHook,
                Payload::MetalDeviceMemSample {
                    allocated_bytes: 100,
                    recommended_max_bytes: 1000,
                    at_event: "scope_enter".into(),
                },
            ),
            ev(
                1,
                Source::MetalHook,
                Payload::MetalDeviceMemSample {
                    allocated_bytes: 300,
                    recommended_max_bytes: 1000,
                    at_event: "scope_exit".into(),
                },
            ),
            ev(
                2,
                Source::Mark,
                Payload::Mark {
                    label: "x".into(),
                    fields: Default::default(),
                },
            ),
        ];
        // new cursor = 3; current = last sample (300), peak = 300
        let r = summarize_delta(&evs, 2, "sid".into(), None, true);
        assert_eq!(r.memory.current_bytes, 300);
        assert_eq!(r.memory.peak_bytes_session, 300);
    }

    #[test]
    fn model_loads_in_delta_use_basename() {
        let evs = vec![
            ev(
                0,
                Source::Mark,
                Payload::Mark {
                    label: "x".into(),
                    fields: Default::default(),
                },
            ),
            ev(
                1,
                Source::PythonSidecar,
                Payload::ModelLoad {
                    path: "/models/llama/weights.safetensors".into(),
                    size_bytes: 4_096,
                    t_start_ns: 0,
                    t_end_ns: 10,
                    sha8: None,
                    framework: Some("safetensors".into()),
                },
            ),
        ];
        let r = summarize_delta(&evs, 1, "sid".into(), None, true);
        assert_eq!(
            r.model_loads,
            vec![ModelLoadDelta {
                name: "weights.safetensors".into(),
                bytes: 4_096
            }]
        );
    }

    fn mark(seq: u64, ts: u64) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq,
            payload: Payload::Mark {
                label: format!("m-{seq}"),
                fields: Default::default(),
            },
        }
    }

    #[test]
    fn baseline_counts_everything() {
        let evs: Vec<Event> = (0..5).map(|i| mark(i, i * 100)).collect();
        let r = summarize_delta(&evs, 0, "sid".into(), None, true);
        assert_eq!(r.prev_cursor, 0);
        assert_eq!(r.cursor, 5);
        assert_eq!(r.new_events, 5);
        assert_eq!(r.elapsed_ns, 400);
        assert_eq!(
            r.by_payload,
            vec![PayloadCount {
                kind: "Mark".into(),
                count: 5
            }]
        );
        assert_eq!(r.note, None);
    }

    #[test]
    fn delta_counts_only_after_cursor() {
        let evs: Vec<Event> = (0..5).map(|i| mark(i, i * 100)).collect();
        let r = summarize_delta(&evs, 3, "sid".into(), None, true);
        assert_eq!(r.prev_cursor, 3);
        assert_eq!(r.cursor, 5);
        assert_eq!(r.new_events, 2);
    }

    #[test]
    fn empty_delta_notes_no_new_events() {
        let evs: Vec<Event> = (0..5).map(|i| mark(i, i * 100)).collect();
        let r = summarize_delta(&evs, 5, "sid".into(), None, true);
        assert_eq!(r.new_events, 0);
        assert_eq!(r.note.as_deref(), Some("no new events"));
    }

    #[test]
    fn cursor_past_end_is_clamped_not_panic() {
        let evs: Vec<Event> = (0..3).map(|i| mark(i, i * 100)).collect();
        let r = summarize_delta(&evs, 99, "sid".into(), None, true);
        assert_eq!(r.new_events, 0);
        assert_eq!(r.cursor, 3);
        assert_eq!(r.note.as_deref(), Some("cursor ahead of log (clamped)"));
    }

    #[test]
    fn finalized_session_notes_finalized() {
        let evs: Vec<Event> = (0..5).map(|i| mark(i, i * 100)).collect();
        let r = summarize_delta(&evs, 3, "sid".into(), None, false);
        assert_eq!(r.note.as_deref(), Some("session finalized"));
    }

    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;

    fn write_live_session(n: u64) -> SessionId {
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        for i in 0..n {
            w.write_event(&mark(i, i * 100)).unwrap();
        }
        w.flush().unwrap(); // NOT finalize: session stays "live"
        id
    }

    #[test]
    #[serial_test::serial]
    fn run_baseline_then_delta_no_double_count() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = write_live_session(3);

        // First poll: baseline.
        let r1 = run(Params {
            session: Some(id.short()),
            cursor: None,
        })
        .unwrap();
        assert!(r1.live);
        assert_eq!(r1.new_events, 3);
        assert_eq!(r1.cursor, 3);
        let sid = r1.session_id.clone();

        // Second poll from the returned cursor: no overlap.
        let r2 = run(Params {
            session: Some(sid),
            cursor: Some(r1.cursor),
        })
        .unwrap();
        assert_eq!(r2.new_events, 0);
        assert_eq!(r2.note.as_deref(), Some("no new events"));
    }

    #[test]
    #[serial_test::serial]
    fn default_session_picks_most_recent_live() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());

        // One finalized session.
        let id_old = SessionId::new();
        let mut w = SessionWriter::create(SessionMetadata::now_starting(id_old)).unwrap();
        w.write_event(&mark(0, 0)).unwrap();
        w.finalize(Some(0), "x".into()).unwrap();

        // One live session (more recent).
        let id_live = write_live_session(2);

        let r = run(Params {
            session: None,
            cursor: None,
        })
        .unwrap();
        assert!(r.live);
        assert_eq!(r.session_id, id_live.to_string());
        // round-trips through resolve_session
        let again = run(Params {
            session: Some(r.session_id.clone()),
            cursor: None,
        })
        .unwrap();
        assert_eq!(again.session_id, id_live.to_string());
    }

    #[test]
    #[serial_test::serial]
    fn default_session_falls_back_to_finalized_with_live_false() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let mut w = SessionWriter::create(SessionMetadata::now_starting(id)).unwrap();
        w.write_event(&mark(0, 0)).unwrap();
        w.finalize(Some(0), "x".into()).unwrap();

        let r = run(Params {
            session: None,
            cursor: None,
        })
        .unwrap();
        assert!(!r.live);
        assert_eq!(r.note.as_deref(), Some("session finalized"));
    }
}

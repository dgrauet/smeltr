//! Export a recorded session to structured formats.
//!
//! Two formats are supported:
//! - `to_json_raw`: pretty-printed JSON with `{ metadata, events }`.
//! - `to_chrome_trace`: Trace Event Format (consumed by chrome://tracing,
//!   Perfetto, Speedscope).

use serde_json::{json, Value};
use smeltr_core::event::{Event, FieldValue, Payload};
use smeltr_core::session::SessionMetadata;
use std::collections::{BTreeMap, HashMap};

/// Stored entry for an in-flight `ModuleEntered` event.
/// (t_enter_us, qualname, class_name, depth, fields)
type OpenModule = (f64, String, String, u16, BTreeMap<String, FieldValue>);

/// Pretty-printed JSON dump of all events plus session metadata.
pub fn to_json_raw(events: &[Event], meta: &SessionMetadata) -> String {
    let v = json!({
        "metadata": meta,
        "events": events,
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string())
}

/// Convert a session to the Chrome Trace Event Format (single-object form
/// with `traceEvents` array).
///
/// Mapping summary (full table in spec):
/// - Module/Scope pairs    → ph="X" on pid=1 "Python", tid=depth
/// - MetalCb pairs         → ph="X" on pid=2 "Metal CBs", tid="queue_{queue_id}"
/// - MetalCbOps entries    → ph="X" on pid=3 "Kernels", tid="cb_{cb_id}",
///   ts=cb_committed.ts, dur=op.gpu_ns/1000
/// - Marks                 → ph="i" instant on pid=1
/// - SessionStarted/Ended  → ph="i" instant on pid=1
///
/// All timestamps in microseconds (ts_mono_ns/1000).
pub fn to_chrome_trace(events: &[Event], meta: &SessionMetadata) -> String {
    let mut trace_events: Vec<Value> = Vec::new();

    // Process-naming metadata events for the three lanes.
    for (pid, name) in &[(1, "Python"), (2, "Metal CBs"), (3, "Kernels")] {
        trace_events.push(json!({
            "ph": "M",
            "name": "process_name",
            "pid": pid,
            "tid": 0,
            "args": { "name": name },
        }));
    }

    // Pair-tracking maps:
    // module_call_id -> (t_enter_us, qualname, class_name, depth, fields)
    let mut open_modules: HashMap<u64, OpenModule> = HashMap::new();
    // cb_id -> (t_commit_us, queue_id, label)
    let mut open_cbs: HashMap<u64, (f64, u64, Option<String>)> = HashMap::new();
    // cb_id -> Vec<OpSample> queued for emission once the CB completes
    // (so we can use the committed ts for the op's ts).
    let mut pending_ops: HashMap<u64, Vec<smeltr_core::event::OpSample>> = HashMap::new();

    for ev in events {
        let ts_us = ev.ts_mono_ns as f64 / 1000.0;
        match &ev.payload {
            Payload::ModuleEntered {
                module_call_id,
                qualname,
                class_name,
                depth,
                fields,
                ..
            } => {
                open_modules.insert(
                    *module_call_id,
                    (
                        ts_us,
                        qualname.clone(),
                        class_name.clone(),
                        *depth,
                        fields.clone(),
                    ),
                );
            }
            Payload::ModuleReturned { module_call_id } => {
                if let Some((t_enter, qualname, class_name, depth, fields)) =
                    open_modules.remove(module_call_id)
                {
                    let dur = ts_us - t_enter;
                    let mut args = serde_json::Map::new();
                    args.insert("class_name".into(), serde_json::Value::String(class_name));
                    args.insert("module_call_id".into(), json!(module_call_id));
                    for (k, v) in fields {
                        args.insert(
                            k,
                            serde_json::to_value(v).unwrap_or(serde_json::Value::Null),
                        );
                    }
                    trace_events.push(json!({
                        "ph": "X",
                        "name": qualname,
                        "pid": 1,
                        "tid": depth,
                        "ts": t_enter,
                        "dur": dur,
                        "args": args,
                    }));
                }
            }
            Payload::MetalCbCommitted {
                cb_id,
                queue_id,
                label,
                ..
            } => {
                open_cbs.insert(*cb_id, (ts_us, *queue_id, label.clone()));
            }
            Payload::MetalCbCompleted {
                cb_id,
                status,
                error_code,
                error_domain,
                in_flight_ns,
                ..
            } => {
                if let Some((t_commit, queue_id, label)) = open_cbs.remove(cb_id) {
                    let name = label.unwrap_or_else(|| format!("cb_{cb_id}"));
                    let dur = *in_flight_ns as f64 / 1000.0;
                    trace_events.push(json!({
                        "ph": "X",
                        "name": name,
                        "pid": 2,
                        "tid": format!("queue_{queue_id}"),
                        "ts": t_commit,
                        "dur": dur,
                        "args": {
                            "cb_id": cb_id,
                            "status": status,
                            "error_code": error_code,
                            "error_domain": error_domain,
                        },
                    }));
                    if let Some(ops) = pending_ops.remove(cb_id) {
                        for op in ops {
                            let op_name = op.symbol.clone().unwrap_or_else(|| op.name.clone());
                            trace_events.push(json!({
                                "ph": "X",
                                "name": op_name,
                                "pid": 3,
                                "tid": format!("cb_{cb_id}"),
                                "ts": t_commit,
                                "dur": op.gpu_ns as f64 / 1000.0,
                                "args": {
                                    "raw_name": op.name,
                                    "symbol": op.symbol,
                                    "count": op.count,
                                },
                            }));
                        }
                    }
                }
            }
            Payload::MetalCbOps { cb_id, ops } => {
                pending_ops.entry(*cb_id).or_default().extend(ops.clone());
            }
            Payload::Mark { label } => {
                trace_events.push(json!({
                    "ph": "i",
                    "name": label,
                    "pid": 1,
                    "tid": 0,
                    "ts": ts_us,
                    "s": "g",
                }));
            }
            Payload::SessionStarted { .. } => {
                trace_events.push(json!({
                    "ph": "i",
                    "name": "session-started",
                    "pid": 1,
                    "tid": 0,
                    "ts": ts_us,
                    "s": "g",
                }));
            }
            Payload::SessionEnded { reason, .. } => {
                trace_events.push(json!({
                    "ph": "i",
                    "name": format!("session-ended ({reason})"),
                    "pid": 1,
                    "tid": 0,
                    "ts": ts_us,
                    "s": "g",
                }));
            }
            _ => {}
        }
    }

    let v = json!({
        "traceEvents": trace_events,
        "displayTimeUnit": "ms",
        "metadata": meta,
    });
    serde_json::to_string_pretty(&v).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{OpSample, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use uuid::Uuid;

    fn ev(seq: u64, ts: u64, src: Source, payload: Payload) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source: src,
            pid: None,
            seq,
            payload,
        }
    }

    fn parse_trace(s: &str) -> serde_json::Value {
        serde_json::from_str(s).expect("chrome-trace output must be valid JSON")
    }

    #[test]
    fn chrome_trace_includes_three_process_metadata_events() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let s = to_chrome_trace(&[], &meta);
        let v = parse_trace(&s);
        let names: Vec<String> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"] == "M")
            .map(|e| e["args"]["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["Python", "Metal CBs", "Kernels"]);
    }

    #[test]
    fn chrome_trace_empty_session_is_valid_json_with_only_metadata() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let s = to_chrome_trace(&[], &meta);
        let v = parse_trace(&s);
        let non_meta: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"] != "M")
            .collect();
        assert!(non_meta.is_empty(), "no events expected, got {non_meta:?}");
    }

    #[test]
    fn chrome_trace_module_pair_becomes_complete_event() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let evs = vec![
            ev(
                1,
                1_000,
                Source::PythonSidecar,
                Payload::ModuleEntered {
                    module_call_id: 7,
                    module_def_id: 0,
                    qualname: "denoise.pass:cond".into(),
                    class_name: "Scope".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields: Default::default(),
                },
            ),
            ev(
                2,
                3_000,
                Source::PythonSidecar,
                Payload::ModuleReturned { module_call_id: 7 },
            ),
        ];
        let s = to_chrome_trace(&evs, &meta);
        let v = parse_trace(&s);
        let x: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"] == "X")
            .collect();
        assert_eq!(x.len(), 1);
        assert_eq!(x[0]["name"], "denoise.pass:cond");
        assert_eq!(x[0]["pid"], 1);
        assert_eq!(x[0]["tid"], 0);
        assert_eq!(x[0]["ts"], 1.0);
        assert_eq!(x[0]["dur"], 2.0);
        assert_eq!(x[0]["args"]["class_name"], "Scope");
    }

    #[test]
    fn chrome_trace_unmatched_module_return_is_skipped() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let evs = vec![ev(
            1,
            1_000,
            Source::PythonSidecar,
            Payload::ModuleReturned {
                module_call_id: 999,
            },
        )];
        let s = to_chrome_trace(&evs, &meta);
        let v = parse_trace(&s);
        let x: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"] == "X")
            .collect();
        assert!(x.is_empty(), "orphan ModuleReturned must not emit an event");
    }

    #[test]
    fn chrome_trace_cb_pair_becomes_complete_event() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let evs = vec![
            ev(
                1,
                10_000,
                Source::MetalHook,
                Payload::MetalCbCommitted {
                    cb_id: 9,
                    queue_id: 1,
                    queue_depth: 0,
                    label: Some("forward".into()),
                },
            ),
            ev(
                2,
                15_000,
                Source::MetalHook,
                Payload::MetalCbCompleted {
                    cb_id: 9,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 5_000,
                },
            ),
        ];
        let s = to_chrome_trace(&evs, &meta);
        let v = parse_trace(&s);
        let cb_events: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["pid"] == 2 && e["ph"] == "X")
            .collect();
        assert_eq!(cb_events.len(), 1);
        assert_eq!(cb_events[0]["name"], "forward");
        assert_eq!(cb_events[0]["tid"], "queue_1");
        assert_eq!(cb_events[0]["ts"], 10.0);
        assert_eq!(cb_events[0]["dur"], 5.0);
    }

    #[test]
    fn chrome_trace_ops_emit_on_kernels_lane_at_commit_ts() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let evs = vec![
            ev(
                1,
                10_000,
                Source::MetalHook,
                Payload::MetalCbCommitted {
                    cb_id: 9,
                    queue_id: 1,
                    queue_depth: 0,
                    label: None,
                },
            ),
            ev(
                2,
                11_000,
                Source::MetalHook,
                Payload::MetalCbOps {
                    cb_id: 9,
                    ops: vec![OpSample {
                        name: "K_abcd_64x64x1".into(),
                        symbol: Some("gemm_t_n_bf16_64_64_32".into()),
                        gpu_ns: 2_000,
                        count: 1,
                    }],
                },
            ),
            ev(
                3,
                15_000,
                Source::MetalHook,
                Payload::MetalCbCompleted {
                    cb_id: 9,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 5_000,
                },
            ),
        ];
        let s = to_chrome_trace(&evs, &meta);
        let v = parse_trace(&s);
        let kernel_events: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["pid"] == 3 && e["ph"] == "X")
            .collect();
        assert_eq!(kernel_events.len(), 1);
        assert_eq!(kernel_events[0]["name"], "gemm_t_n_bf16_64_64_32");
        assert_eq!(kernel_events[0]["tid"], "cb_9");
        assert_eq!(kernel_events[0]["ts"], 10.0);
        assert_eq!(kernel_events[0]["dur"], 2.0);
        assert_eq!(kernel_events[0]["args"]["raw_name"], "K_abcd_64x64x1");
    }

    #[test]
    fn chrome_trace_mark_becomes_instant_event() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let evs = vec![ev(
            1,
            7_000,
            Source::Mark,
            Payload::Mark {
                label: "step-1".into(),
            },
        )];
        let s = to_chrome_trace(&evs, &meta);
        let v = parse_trace(&s);
        let i: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"] == "i")
            .collect();
        assert_eq!(i.len(), 1);
        assert_eq!(i[0]["name"], "step-1");
        assert_eq!(i[0]["ts"], 7.0);
        assert_eq!(i[0]["s"], "g");
    }

    #[test]
    fn json_raw_round_trips_events() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let evs = vec![ev(
            1,
            5_000,
            Source::Mark,
            Payload::Mark {
                label: "hello".into(),
            },
        )];
        let s = to_json_raw(&evs, &meta);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["events"].as_array().unwrap().len(), 1);
        assert_eq!(
            v["events"][0]["payload"]["label"].as_str().unwrap(),
            "hello"
        );
        assert!(v["metadata"]["session_id"].is_string());
    }

    #[test]
    fn chrome_trace_ops_without_matching_cb_are_dropped() {
        // Ops arrive but the CB never completes (e.g., process crashed mid-CB).
        // Contract: drop the orphan ops; do NOT emit a kernel event.
        let meta = SessionMetadata::now_starting(SessionId::new());
        let evs = vec![ev(
            1,
            11_000,
            Source::MetalHook,
            Payload::MetalCbOps {
                cb_id: 99,
                ops: vec![OpSample {
                    name: "K_orphan_64x64x1".into(),
                    symbol: Some("gemm_orphan".into()),
                    gpu_ns: 1_000,
                    count: 1,
                }],
            },
        )];
        let s = to_chrome_trace(&evs, &meta);
        let v = parse_trace(&s);
        let kernel_events: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["pid"] == 3 && e["ph"] == "X")
            .collect();
        assert!(
            kernel_events.is_empty(),
            "orphan MetalCbOps must not emit kernel events; got {kernel_events:?}"
        );
    }

    #[test]
    fn chrome_trace_includes_session_metadata() {
        let mut meta = SessionMetadata::now_starting(SessionId::new());
        meta.name = Some("ltx2-baseline".into());
        let s = to_chrome_trace(&[], &meta);
        let v = parse_trace(&s);
        assert_eq!(v["metadata"]["name"], "ltx2-baseline");
        assert_eq!(v["displayTimeUnit"], "ms");
    }

    #[test]
    fn chrome_trace_module_entered_with_fields_merges_into_args() {
        use smeltr_core::event::{Event, FieldValue, Payload, Source};
        use std::collections::BTreeMap;
        use uuid::Uuid;

        let mut fields = BTreeMap::new();
        fields.insert("step".into(), FieldValue::Int(5));
        fields.insert("sigma".into(), FieldValue::Float(0.5));

        let events = vec![
            Event {
                ts_mono_ns: 1_000_000,
                ts_wall_ns: 0,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 1,
                payload: Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 0,
                    qualname: "denoise.step".into(),
                    class_name: "Scope".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields,
                },
            },
            Event {
                ts_mono_ns: 2_000_000,
                ts_wall_ns: 0,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 2,
                payload: Payload::ModuleReturned { module_call_id: 1 },
            },
        ];
        let meta = SessionMetadata::now_starting(SessionId::new());
        let s = to_chrome_trace(&events, &meta);
        let v = parse_trace(&s);
        let entries = v["traceEvents"]
            .as_array()
            .expect("traceEvents is an array");
        let evt = entries
            .iter()
            .find(|e| e.get("name").and_then(|n| n.as_str()) == Some("denoise.step"))
            .expect("denoise.step event present");
        let args = evt.get("args").expect("args present").as_object().unwrap();
        assert_eq!(
            args.get("class_name").and_then(|v| v.as_str()),
            Some("Scope")
        );
        assert_eq!(args.get("module_call_id").and_then(|v| v.as_u64()), Some(1));
        assert_eq!(args.get("step").and_then(|v| v.as_i64()), Some(5));
        assert_eq!(args.get("sigma").and_then(|v| v.as_f64()), Some(0.5));
    }

    #[test]
    fn chrome_trace_module_entered_empty_fields_omits_keys() {
        use smeltr_core::event::{Event, Payload, Source};
        use uuid::Uuid;

        let events = vec![
            Event {
                ts_mono_ns: 1_000_000,
                ts_wall_ns: 0,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 1,
                payload: Payload::ModuleEntered {
                    module_call_id: 7,
                    module_def_id: 0,
                    qualname: "plain".into(),
                    class_name: "Scope".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields: Default::default(),
                },
            },
            Event {
                ts_mono_ns: 2_000_000,
                ts_wall_ns: 0,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 2,
                payload: Payload::ModuleReturned { module_call_id: 7 },
            },
        ];
        let meta = SessionMetadata::now_starting(SessionId::new());
        let s = to_chrome_trace(&events, &meta);
        let v = parse_trace(&s);
        let entries = v["traceEvents"].as_array().unwrap();
        let evt = entries
            .iter()
            .find(|e| e.get("name").and_then(|n| n.as_str()) == Some("plain"))
            .unwrap();
        let args = evt.get("args").unwrap().as_object().unwrap();
        assert_eq!(
            args.len(),
            2,
            "only class_name + module_call_id, got {args:?}"
        );
    }
}

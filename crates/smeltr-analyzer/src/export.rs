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

/// Collected ModelLoad event fields for post-loop emission.
/// (path, size_bytes, t_start_ns, t_end_ns, sha8, framework)
type ModelLoadEntry = (String, u64, u64, u64, Option<String>, Option<String>);

/// Collected ModelUnload event fields for post-loop instant emission.
/// (path, t_ns, sha8)
type ModelUnloadEntry = (String, u64, Option<String>);

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

    // Process-naming metadata events for the four lanes.
    for (pid, name) in &[
        (1, "Python"),
        (2, "Metal CBs"),
        (3, "Kernels"),
        (4, "Model Loads"),
    ] {
        trace_events.push(json!({
            "ph": "M",
            "name": "process_name",
            "pid": pid,
            "tid": 0,
            "args": { "name": name },
        }));
    }

    // Collect ModelLoad events for swim-lane and counter emission after
    // the main event loop (counters need global sort by t_end_ns).
    let mut model_loads: Vec<ModelLoadEntry> = Vec::new();
    // Collect ModelUnload events for instant emission.
    let mut model_unloads: Vec<ModelUnloadEntry> = Vec::new();

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
            Payload::Mark { label, fields } => {
                let mut ev = serde_json::json!({
                    "ph": "i",
                    "name": label,
                    "pid": 1,
                    "tid": 0,
                    "ts": ts_us,
                    "s": "g",
                });
                if !fields.is_empty() {
                    let args: serde_json::Map<String, serde_json::Value> = fields
                        .iter()
                        .map(|(k, v)| {
                            let jv = match v {
                                smeltr_core::event::FieldValue::Bool(b) => {
                                    serde_json::Value::Bool(*b)
                                }
                                smeltr_core::event::FieldValue::Int(i) => serde_json::json!(i),
                                smeltr_core::event::FieldValue::Float(f) => serde_json::json!(f),
                                smeltr_core::event::FieldValue::String(s) => {
                                    serde_json::Value::String(s.clone())
                                }
                            };
                            (k.clone(), jv)
                        })
                        .collect();
                    ev["args"] = serde_json::Value::Object(args);
                }
                trace_events.push(ev);
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
            Payload::ModelLoad {
                path,
                size_bytes,
                t_start_ns,
                t_end_ns,
                sha8,
                framework,
            } => {
                model_loads.push((
                    path.clone(),
                    *size_bytes,
                    *t_start_ns,
                    *t_end_ns,
                    sha8.clone(),
                    framework.clone(),
                ));
            }
            Payload::ModelUnload { path, t_ns, sha8 } => {
                model_unloads.push((path.clone(), *t_ns, sha8.clone()));
            }
            Payload::MetalDeviceMemSample {
                allocated_bytes,
                recommended_max_bytes,
                ..
            } => {
                trace_events.push(json!({
                    "ph": "C",
                    "name": "gpu_memory",
                    "pid": 0,
                    "ts": ev.ts_mono_ns as f64 / 1000.0,
                    "args": { "allocated_bytes": allocated_bytes, "budget_bytes": recommended_max_bytes },
                }));
            }
            Payload::VmSample {
                wired_bytes,
                active_bytes,
                compressed_bytes,
                swap_used_bytes,
                page_outs_per_sec,
            } => {
                let ts = ev.ts_mono_ns as f64 / 1000.0;
                trace_events.push(json!({
                    "ph": "C",
                    "name": "system_memory",
                    "pid": 0,
                    "ts": ts,
                    "args": {
                        "wired_bytes": wired_bytes,
                        "active_bytes": active_bytes,
                        "compressed_bytes": compressed_bytes,
                        "swap_used_bytes": swap_used_bytes,
                    },
                }));
                if page_outs_per_sec.is_finite() {
                    trace_events.push(json!({
                        "ph": "C",
                        "name": "vm_page_outs_per_sec",
                        "pid": 0,
                        "ts": ts,
                        "args": { "rate": page_outs_per_sec },
                    }));
                }
            }
            Payload::ThermalState { level } => {
                trace_events.push(json!({
                    "ph": "C",
                    "name": "thermal_level",
                    "pid": 0,
                    "ts": ev.ts_mono_ns as f64 / 1000.0,
                    "args": { "level": level },
                }));
            }
            Payload::IoReportSample {
                gpu_residency_pct,
                ane_residency_pct,
                cpu_residency_pct,
                gpu_power_mw,
                gpu_freq_mhz,
            } => {
                let ts = ev.ts_mono_ns as f64 / 1000.0;
                let mut util = serde_json::Map::new();
                if let Some(p) = gpu_residency_pct {
                    if p.is_finite() {
                        util.insert("gpu".into(), json!(p));
                    }
                }
                if let Some(p) = ane_residency_pct {
                    if p.is_finite() {
                        util.insert("ane".into(), json!(p));
                    }
                }
                if let Some(p) = cpu_residency_pct {
                    if p.is_finite() {
                        util.insert("cpu".into(), json!(p));
                    }
                }
                if !util.is_empty() {
                    trace_events.push(json!({
                        "ph": "C",
                        "name": "utilization_pct",
                        "pid": 0,
                        "ts": ts,
                        "args": util,
                    }));
                }
                if let Some(mw) = gpu_power_mw {
                    trace_events.push(json!({
                        "ph": "C",
                        "name": "gpu_power_mw",
                        "pid": 0,
                        "ts": ts,
                        "args": { "mw": mw },
                    }));
                }
                if let Some(mhz) = gpu_freq_mhz {
                    trace_events.push(json!({
                        "ph": "C",
                        "name": "gpu_freq_mhz",
                        "pid": 0,
                        "ts": ts,
                        "args": { "mhz": mhz },
                    }));
                }
            }
            _ => {}
        }
    }

    // Emit ModelUnload instant events (ph:"i") on pid=4.
    for (path, t_ns, sha8) in &model_unloads {
        let basename = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path.as_str());
        let ts_us = *t_ns as f64 / 1000.0;
        let mut args = serde_json::Map::new();
        args.insert("path".into(), serde_json::Value::String(path.clone()));
        if let Some(s) = sha8 {
            args.insert("sha8".into(), serde_json::Value::String(s.clone()));
        }
        trace_events.push(json!({
            "ph": "i",
            "name": format!("unload:{basename}"),
            "cat": "model-unload",
            "pid": 4,
            "tid": 0,
            "ts": ts_us,
            "args": args,
            "s": "p",
        }));
    }

    // Emit ModelLoad swim-lane events (ph:"X") on pid=4.
    for (path, size_bytes, t_start_ns, t_end_ns, sha8, framework) in &model_loads {
        let basename = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path.as_str());
        let ts_us = *t_start_ns as f64 / 1000.0;
        let dur_us = (*t_end_ns - *t_start_ns) as f64 / 1000.0;
        let mut args = serde_json::Map::new();
        args.insert("path".into(), serde_json::Value::String(path.clone()));
        args.insert("size_bytes".into(), json!(size_bytes));
        if let Some(s) = sha8 {
            args.insert("sha8".into(), serde_json::Value::String(s.clone()));
        }
        if let Some(f) = framework {
            args.insert("framework".into(), serde_json::Value::String(f.clone()));
        }
        trace_events.push(json!({
            "ph": "X",
            "name": basename,
            "cat": "model-load",
            "pid": 4,
            "tid": 0,
            "ts": ts_us,
            "dur": dur_us,
            "args": args,
        }));
    }

    // Emit per-model counter tracks (ph:"C") — cumulative bytes, sorted by t_end_ns.
    // Key: sha8 if present, else canonical path. Counter name: "model:<basename>".
    {
        let mut sorted = model_loads.clone();
        sorted.sort_by_key(|(_, _, _, t_end, _, _)| *t_end);
        // cumulative bytes per (key -> basename, cumulative_bytes)
        let mut cumulative: HashMap<String, (String, u64)> = HashMap::new();
        for (path, size_bytes, _t_start, t_end_ns, sha8, _framework) in &sorted {
            let key = sha8.clone().unwrap_or_else(|| path.clone());
            let basename = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path.as_str())
                .to_string();
            let ts_us = *t_end_ns as f64 / 1000.0;
            let entry = cumulative.entry(key).or_insert((basename.clone(), 0));
            entry.1 += size_bytes;
            let counter_name = format!("model:{}", entry.0);
            let cum_bytes = entry.1;
            trace_events.push(json!({
                "ph": "C",
                "name": counter_name,
                "pid": 0,
                "ts": ts_us,
                "args": { "bytes": cum_bytes },
            }));
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
    fn chrome_trace_includes_four_process_metadata_events() {
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
        assert_eq!(names, vec!["Python", "Metal CBs", "Kernels", "Model Loads"]);
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
                fields: Default::default(),
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
                fields: Default::default(),
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
    fn chrome_trace_model_load_becomes_complete_event_on_lane_4() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let evs = vec![ev(
            1,
            5_000_000, // ts_mono_ns (used for ts_us in the event loop, not for t_start_ns)
            Source::PythonSidecar,
            Payload::ModelLoad {
                path: "/models/llama/weights.safetensors".into(),
                size_bytes: 1_048_576,
                t_start_ns: 1_000_000,
                t_end_ns: 3_000_000,
                sha8: Some("ab12cd34".into()),
                framework: Some("safetensors".into()),
            },
        )];
        let s = to_chrome_trace(&evs, &meta);
        let v = parse_trace(&s);
        let x: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"] == "X" && e["pid"] == 4)
            .collect();
        assert_eq!(x.len(), 1, "expected one ph:X on pid:4, got {x:?}");
        assert_eq!(x[0]["name"], "weights.safetensors");
        assert_eq!(x[0]["cat"], "model-load");
        assert_eq!(x[0]["tid"], 0);
        // ts = t_start_ns / 1000.0 = 1_000_000 / 1000.0 = 1000.0 µs
        assert_eq!(x[0]["ts"], 1000.0);
        // dur = (t_end_ns - t_start_ns) / 1000.0 = (3_000_000 - 1_000_000) / 1000.0 = 2000.0 µs
        assert_eq!(x[0]["dur"], 2000.0);
        assert_eq!(x[0]["args"]["path"], "/models/llama/weights.safetensors");
        assert_eq!(x[0]["args"]["size_bytes"], 1_048_576_u64);
        assert_eq!(x[0]["args"]["sha8"], "ab12cd34");
        assert_eq!(x[0]["args"]["framework"], "safetensors");
    }

    #[test]
    fn chrome_trace_model_load_emits_counter_with_cumulative_bytes() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let evs = vec![
            ev(
                1,
                5_000_000,
                Source::PythonSidecar,
                Payload::ModelLoad {
                    path: "/models/llama/weights.safetensors".into(),
                    size_bytes: 1_048_576, // 1 MB
                    t_start_ns: 1_000_000,
                    t_end_ns: 2_000_000,
                    sha8: Some("ab12cd34".into()),
                    framework: None,
                },
            ),
            ev(
                2,
                6_000_000,
                Source::PythonSidecar,
                Payload::ModelLoad {
                    path: "/models/llama/weights.safetensors".into(),
                    size_bytes: 2_097_152, // 2 MB
                    t_start_ns: 3_000_000,
                    t_end_ns: 4_000_000,
                    sha8: Some("ab12cd34".into()),
                    framework: None,
                },
            ),
        ];
        let s = to_chrome_trace(&evs, &meta);
        let v = parse_trace(&s);
        let counters: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"] == "C")
            .collect();
        assert_eq!(
            counters.len(),
            2,
            "expected 2 counter events, got {counters:?}"
        );
        // Sorted by t_end_ns: first at ts=t_end_ns/1000=2000.0 µs, bytes=1MB;
        // second at ts=4000.0 µs, bytes=3MB cumulative.
        assert_eq!(counters[0]["ts"], 2000.0);
        assert_eq!(counters[0]["args"]["bytes"], 1_048_576_u64);
        assert_eq!(counters[1]["ts"], 4000.0);
        assert_eq!(counters[1]["args"]["bytes"], 3_145_728_u64);
        // Both use same counter name
        assert_eq!(counters[0]["name"], counters[1]["name"]);
    }

    #[test]
    fn chrome_trace_model_load_distinct_models_have_distinct_counter_names() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let evs = vec![
            ev(
                1,
                1_000_000,
                Source::PythonSidecar,
                Payload::ModelLoad {
                    path: "/models/llama/weights.safetensors".into(),
                    size_bytes: 1_000,
                    t_start_ns: 1_000_000,
                    t_end_ns: 2_000_000,
                    sha8: Some("aaaa1111".into()),
                    framework: None,
                },
            ),
            ev(
                2,
                3_000_000,
                Source::PythonSidecar,
                Payload::ModelLoad {
                    path: "/models/mistral/model.safetensors".into(),
                    size_bytes: 2_000,
                    t_start_ns: 3_000_000,
                    t_end_ns: 4_000_000,
                    sha8: Some("bbbb2222".into()),
                    framework: None,
                },
            ),
        ];
        let s = to_chrome_trace(&evs, &meta);
        let v = parse_trace(&s);
        let counters: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"] == "C")
            .collect();
        assert_eq!(counters.len(), 2);
        let names: std::collections::HashSet<String> = counters
            .iter()
            .map(|e| e["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names.len(),
            2,
            "distinct models must have distinct counter names"
        );
        assert!(names.iter().any(|n| n.contains("weights.safetensors")));
        assert!(names.iter().any(|n| n.contains("model.safetensors")));
    }

    #[test]
    fn chrome_trace_model_load_omits_none_args() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let evs = vec![ev(
            1,
            5_000_000,
            Source::PythonSidecar,
            Payload::ModelLoad {
                path: "/models/anon/weights.bin".into(),
                size_bytes: 512,
                t_start_ns: 1_000_000,
                t_end_ns: 2_000_000,
                sha8: None,
                framework: None,
            },
        )];
        let s = to_chrome_trace(&evs, &meta);
        let v = parse_trace(&s);
        let x: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"] == "X" && e["pid"] == 4)
            .collect();
        assert_eq!(x.len(), 1);
        let args = x[0]["args"].as_object().unwrap();
        assert!(
            !args.contains_key("sha8"),
            "sha8 None must be omitted from args"
        );
        assert!(
            !args.contains_key("framework"),
            "framework None must be omitted from args"
        );
        assert_eq!(args["path"], "/models/anon/weights.bin");
        assert_eq!(args["size_bytes"], 512_u64);
    }

    #[test]
    fn chrome_trace_model_load_includes_lane_4_process_metadata() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let s = to_chrome_trace(&[], &meta);
        let v = parse_trace(&s);
        let pid4_meta: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"] == "M" && e["pid"] == 4)
            .collect();
        assert_eq!(pid4_meta.len(), 1);
        assert_eq!(pid4_meta[0]["args"]["name"], "Model Loads");
    }

    #[test]
    fn chrome_trace_model_unload_emits_instant_on_pid4() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let evs = vec![
            ev(
                1,
                1_000_000,
                Source::PythonSidecar,
                Payload::ModelLoad {
                    path: "/models/llama/weights.safetensors".into(),
                    size_bytes: 1_048_576,
                    t_start_ns: 1_000_000,
                    t_end_ns: 2_000_000,
                    sha8: Some("ab12cd34".into()),
                    framework: None,
                },
            ),
            ev(
                2,
                5_000_000,
                Source::PythonSidecar,
                Payload::ModelUnload {
                    path: "/models/llama/weights.safetensors".into(),
                    t_ns: 5_000_000,
                    sha8: Some("ab12cd34".into()),
                },
            ),
        ];
        let s = to_chrome_trace(&evs, &meta);
        let v = parse_trace(&s);
        let instants: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"] == "i" && e["pid"] == 4 && e["cat"] == "model-unload")
            .collect();
        assert_eq!(
            instants.len(),
            1,
            "expected one ph:i on pid:4, got {instants:?}"
        );
        assert_eq!(instants[0]["name"], "unload:weights.safetensors");
        // ts = t_ns / 1000.0 = 5_000_000 / 1000.0 = 5000.0 µs
        assert_eq!(instants[0]["ts"], 5000.0);
        assert_eq!(instants[0]["s"], "p");
        assert_eq!(
            instants[0]["args"]["path"],
            "/models/llama/weights.safetensors"
        );
        assert_eq!(instants[0]["args"]["sha8"], "ab12cd34");
    }

    #[test]
    fn chrome_trace_emits_system_counters() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let evs = vec![
            ev(
                1,
                1_000,
                Source::Vm,
                Payload::VmSample {
                    wired_bytes: 100,
                    active_bytes: 200,
                    compressed_bytes: 50,
                    swap_used_bytes: 10,
                    page_outs_per_sec: 1.5,
                },
            ),
            ev(
                2,
                2_000,
                Source::Thermal,
                Payload::ThermalState { level: 2 },
            ),
            ev(
                3,
                3_000,
                Source::IoReport,
                Payload::IoReportSample {
                    gpu_residency_pct: Some(80.0),
                    ane_residency_pct: None,
                    cpu_residency_pct: Some(40.0),
                    gpu_power_mw: Some(1500),
                    gpu_freq_mhz: None,
                },
            ),
        ];
        let v = parse_trace(&to_chrome_trace(&evs, &meta));
        let counters: Vec<_> = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["ph"] == "C")
            .collect();
        let by_name = |n: &str| counters.iter().find(|e| e["name"] == n);

        let mem = by_name("system_memory").expect("system_memory");
        assert_eq!(mem["args"]["wired_bytes"], 100);
        assert_eq!(mem["args"]["swap_used_bytes"], 10);

        assert!(by_name("vm_page_outs_per_sec").is_some());
        assert_eq!(by_name("thermal_level").unwrap()["args"]["level"], 2);

        let util = by_name("utilization_pct").expect("utilization_pct");
        assert_eq!(util["args"]["gpu"], 80.0);
        assert_eq!(util["args"]["cpu"], 40.0);
        assert!(util["args"].get("ane").is_none(), "None pct must be absent");

        assert_eq!(by_name("gpu_power_mw").unwrap()["args"]["mw"], 1500);
        assert!(
            by_name("gpu_freq_mhz").is_none(),
            "None field skips its track"
        );
    }

    #[test]
    fn chrome_trace_skips_non_finite_f32() {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let evs = vec![ev(
            1,
            1_000,
            Source::Vm,
            Payload::VmSample {
                wired_bytes: 1,
                active_bytes: 1,
                compressed_bytes: 1,
                swap_used_bytes: 1,
                page_outs_per_sec: f32::NAN,
            },
        )];
        // Must not panic and must produce valid JSON (NaN would otherwise fail
        // serde_json::to_string and break the whole export).
        let s = to_chrome_trace(&evs, &meta);
        let v = parse_trace(&s);
        let has_rate = v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "vm_page_outs_per_sec");
        assert!(!has_rate, "non-finite page_outs_per_sec must be skipped");
        // system_memory still emitted (its fields are finite u64s)
        assert!(v["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "system_memory"));
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

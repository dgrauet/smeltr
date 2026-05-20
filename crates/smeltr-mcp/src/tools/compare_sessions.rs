//! `compare_sessions` tool: side-by-side stats of two sessions.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_analyzer::diff::{
    diff_memory, diff_origins, diff_sessions, MemoryDelta, OpDelta, OriginDelta, ScopeAggregate,
    ScopeDelta,
};
use smeltr_core::event::{Event, Source};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    pub session_a: String,
    pub session_b: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub a: SessionStats,
    pub b: SessionStats,
    pub delta: DeltaStats,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope_deltas: Vec<ScopeDelta>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub op_deltas: Vec<OpDelta>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes_only_in_a: Vec<ScopeAggregate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes_only_in_b: Vec<ScopeAggregate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_deltas: Vec<MemoryDelta>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub origin_deltas: Vec<OriginDelta>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionStats {
    pub session_id: String,
    pub event_count: usize,
    pub duration_ns: u64,
    pub source_counts: HashMap<String, usize>,
    pub root_cause_title: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeltaStats {
    pub event_count_diff: i64,
    pub duration_diff_ns: i64,
    pub root_cause_match: bool,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let (a, a_events) = stats(&params.session_a)?;
    let (b, b_events) = stats(&params.session_b)?;
    let event_count_diff = b.event_count as i64 - a.event_count as i64;
    let duration_diff_ns = b.duration_ns as i64 - a.duration_ns as i64;
    let root_cause_match = a.root_cause_title == b.root_cause_title;
    let diff = diff_sessions(&a_events, &b_events);
    let memory_deltas = diff_memory(&a_events, &b_events);
    let origin_deltas = diff_origins(&a_events, &b_events);
    Ok(Response {
        a,
        b,
        delta: DeltaStats {
            event_count_diff,
            duration_diff_ns,
            root_cause_match,
        },
        scope_deltas: diff.scope_deltas,
        op_deltas: diff.op_deltas,
        scopes_only_in_a: diff.scopes_only_in_a,
        scopes_only_in_b: diff.scopes_only_in_b,
        memory_deltas,
        origin_deltas,
    })
}

fn stats(arg: &str) -> Result<(SessionStats, Vec<Event>), ToolError> {
    let dir = resolve_session(arg)?;
    let events = smeltr_core::reader::read_events(&dir)?;
    let stats = stats_from_events(&dir, &events);
    Ok((stats, events))
}

fn stats_from_events(dir: &std::path::Path, events: &[Event]) -> SessionStats {
    let duration_ns = if events.len() < 2 {
        0
    } else {
        events
            .last()
            .unwrap()
            .ts_mono_ns
            .saturating_sub(events.first().unwrap().ts_mono_ns)
    };
    let mut counts: HashMap<String, usize> = HashMap::new();
    for ev in events {
        *counts.entry(source_str(&ev.source).into()).or_insert(0) += 1;
    }
    let report = smeltr_analyzer::analyze(events);
    let root_cause_title = report.root_cause().map(|f| f.title.clone());
    SessionStats {
        session_id: dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string(),
        event_count: events.len(),
        duration_ns,
        source_counts: counts,
        root_cause_title,
    }
}

fn source_str(s: &Source) -> &'static str {
    match s {
        Source::Mark => "Mark",
        Source::System => "System",
        Source::IoReport => "IoReport",
        Source::Vm => "Vm",
        Source::Proc => "Proc",
        Source::OsLog => "OsLog",
        Source::Thermal => "Thermal",
        Source::MachExc => "MachExc",
        Source::CrashReport => "CrashReport",
        Source::MetalHook => "MetalHook",
        Source::PythonSidecar => "PythonSidecar",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Event, OpSample, Payload};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    fn make_session(label: &str) -> SessionId {
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        w.write_event(&Event {
            ts_mono_ns: 1_000_000_000,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: 1,
            payload: Payload::Mark {
                label: label.into(),
            },
        })
        .unwrap();
        w.finalize(Some(0), "x".into()).unwrap();
        id
    }

    #[test]
    #[serial_test::serial]
    fn compares_two_sessions() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let a = make_session("a");
        let b = make_session("b");
        let resp = run(Params {
            session_a: a.short(),
            session_b: b.short(),
        })
        .unwrap();
        assert!(resp.a.event_count >= 1);
        assert!(resp.b.event_count >= 1);
        assert!(resp.delta.root_cause_match); // Both have no root cause -> match.
    }

    fn ev(seq: u64, ts: u64, source: Source, payload: Payload) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source,
            pid: None,
            seq,
            payload,
        }
    }

    fn write_session_with_scope(qualname: &str, in_flight_ns: u64, op_gpu_ns: u64) -> SessionId {
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        let evs = vec![
            ev(
                1,
                1,
                Source::PythonSidecar,
                Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 1,
                    qualname: qualname.into(),
                    class_name: "Scope".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields: Default::default(),
                },
            ),
            ev(
                2,
                10,
                Source::PythonSidecar,
                Payload::MlxEvalEntered {
                    call_id: 1,
                    array_count: 1,
                    stream: "gpu".into(),
                    module_stack: vec![1],
                    stack_frames: vec![],
                },
            ),
            ev(
                3,
                20,
                Source::MetalHook,
                Payload::MetalCbCommitted {
                    cb_id: 9,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
            ),
            ev(
                4,
                30,
                Source::MetalHook,
                Payload::MetalCbCompleted {
                    cb_id: 9,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns,
                },
            ),
            ev(
                5,
                31,
                Source::MetalHook,
                Payload::MetalCbOps {
                    cb_id: 9,
                    ops: vec![OpSample {
                        name: "K_abcd_64x64x1".into(),
                        symbol: Some("gemm_t_n_bf16_64".into()),
                        gpu_ns: op_gpu_ns,
                        count: 1,
                    }],
                },
            ),
            ev(
                6,
                40,
                Source::PythonSidecar,
                Payload::MlxEvalReturned {
                    call_id: 1,
                    duration_ns: 30,
                    was_async: false,
                },
            ),
            ev(
                7,
                50,
                Source::PythonSidecar,
                Payload::ModuleReturned { module_call_id: 1 },
            ),
        ];
        for e in &evs {
            w.write_event(e).unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();
        id
    }

    #[test]
    #[serial_test::serial]
    fn compare_includes_scope_deltas() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let a = write_session_with_scope("denoise.pass:cond", 1_000, 800);
        let b = write_session_with_scope("denoise.pass:cond", 500, 400);

        let resp = run(Params {
            session_a: a.short(),
            session_b: b.short(),
        })
        .unwrap();

        let scope = resp
            .scope_deltas
            .iter()
            .find(|s| s.qualname == "denoise.pass:cond")
            .expect("scope present");
        assert!(scope.delta_ns < 0, "B is faster than A");
        assert_eq!(scope.a_gpu_ns, 1_000);
        assert_eq!(scope.b_gpu_ns, 500);
    }

    #[test]
    #[serial_test::serial]
    fn compare_includes_op_deltas() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let a = write_session_with_scope("scope", 1_000, 800);
        let b = write_session_with_scope("scope", 500, 400);

        let resp = run(Params {
            session_a: a.short(),
            session_b: b.short(),
        })
        .unwrap();

        let mm = resp
            .op_deltas
            .iter()
            .find(|o| o.kind == "Matmul")
            .expect("Matmul present");
        assert_eq!(mm.a_gpu_ns, 800);
        assert_eq!(mm.b_gpu_ns, 400);
    }

    #[test]
    #[serial_test::serial]
    fn compare_handles_only_in_a_scopes() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let a = write_session_with_scope("denoise.pass:uncond", 5_000, 4_000);
        let b = write_session_with_scope("denoise.pass:dpo_warmup", 5_000, 4_000);

        let resp = run(Params {
            session_a: a.short(),
            session_b: b.short(),
        })
        .unwrap();

        assert!(resp
            .scopes_only_in_a
            .iter()
            .any(|s| s.qualname == "denoise.pass:uncond"));
        assert!(resp
            .scopes_only_in_b
            .iter()
            .any(|s| s.qualname == "denoise.pass:dpo_warmup"));
    }

    #[test]
    #[serial_test::serial]
    fn compare_backward_compat_existing_fields_still_populated() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let a = write_session_with_scope("scope", 1_000, 800);
        let b = write_session_with_scope("scope", 1_000, 800);

        let resp = run(Params {
            session_a: a.short(),
            session_b: b.short(),
        })
        .unwrap();

        assert!(!resp.a.session_id.is_empty());
        assert!(!resp.b.session_id.is_empty());
        assert!(resp.delta.event_count_diff.abs() <= 1);
    }

    #[test]
    #[serial_test::serial]
    fn compare_includes_memory_deltas() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");

        let mk = |peak: u64| -> SessionId {
            let id = SessionId::new();
            let meta = SessionMetadata::now_starting(id);
            let mut w = SessionWriter::create(meta).unwrap();
            let evs = vec![
                ev(
                    1,
                    1,
                    Source::PythonSidecar,
                    Payload::ModuleEntered {
                        module_call_id: 1,
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
                    2,
                    Source::MetalHook,
                    Payload::MetalDeviceMemSample {
                        allocated_bytes: peak,
                        recommended_max_bytes: 16_000_000_000,
                        at_event: "cb_committed".into(),
                    },
                ),
                ev(
                    3,
                    3,
                    Source::PythonSidecar,
                    Payload::ModuleReturned { module_call_id: 1 },
                ),
            ];
            for e in &evs {
                w.write_event(e).unwrap();
            }
            w.finalize(Some(0), "ok".into()).unwrap();
            id
        };

        let a = mk(2_000_000_000);
        let b = mk(1_000_000_000);
        let resp = run(Params {
            session_a: a.short(),
            session_b: b.short(),
        })
        .unwrap();

        let mem = resp
            .memory_deltas
            .iter()
            .find(|m| m.qualname == "denoise.pass:cond")
            .expect("memory delta present");
        assert!(mem.delta_bytes < 0, "B uses less memory");
        assert_eq!(mem.a_peak_bytes, 2_000_000_000);
        assert_eq!(mem.b_peak_bytes, 1_000_000_000);
    }

    #[test]
    #[serial_test::serial]
    fn compare_includes_origin_deltas() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");

        let mk = |gpu: u64| -> SessionId {
            let id = SessionId::new();
            let meta = SessionMetadata::now_starting(id);
            let mut w = SessionWriter::create(meta).unwrap();
            let evs = vec![
                ev(
                    1,
                    10,
                    Source::PythonSidecar,
                    Payload::MlxEvalEntered {
                        call_id: 1,
                        array_count: 1,
                        stream: "gpu".into(),
                        module_stack: vec![],
                        stack_frames: vec![smeltr_core::event::StackFrame {
                            filename: "/work/attention.py".into(),
                            lineno: 127,
                            funcname: "f".into(),
                        }],
                    },
                ),
                ev(
                    2,
                    15,
                    Source::MetalHook,
                    Payload::MetalCbOps {
                        cb_id: 9,
                        ops: vec![OpSample {
                            name: "K_x".into(),
                            symbol: Some("gemm_bf16".into()),
                            gpu_ns: gpu,
                            count: 1,
                        }],
                    },
                ),
                ev(
                    3,
                    20,
                    Source::PythonSidecar,
                    Payload::MlxEvalReturned {
                        call_id: 1,
                        duration_ns: 10,
                        was_async: false,
                    },
                ),
            ];
            for e in &evs {
                w.write_event(e).unwrap();
            }
            w.finalize(Some(0), "ok".into()).unwrap();
            id
        };

        let a = mk(2_000_000_000);
        let b = mk(1_000_000_000);
        let resp = run(Params {
            session_a: a.short(),
            session_b: b.short(),
        })
        .unwrap();

        let origin = resp
            .origin_deltas
            .iter()
            .find(|o| o.kind == "Matmul" && o.file_line == "attention.py:127")
            .expect("origin delta present");
        assert!(origin.delta_ns < 0);
        assert_eq!(origin.a_gpu_ns, 2_000_000_000);
        assert_eq!(origin.b_gpu_ns, 1_000_000_000);
    }
}

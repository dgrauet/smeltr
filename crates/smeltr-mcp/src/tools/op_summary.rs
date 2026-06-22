//! `get_op_summary` MCP tool: flat cross-module aggregation of GPU ops.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_analyzer::compute_breakdown;
use smeltr_core::reader::read_events;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct Params {
    pub session: String,
    pub top_n: Option<u32>,
    /// Aggregate by `"name"` (default) or `"kind"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OpSummary {
    pub name: String,
    pub gpu_ns: u64,
    pub count: u64,
    pub pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ops: Vec<OpSummary>,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let group_by = match params.group_by.as_deref() {
        None | Some("name") => smeltr_analyzer::OpGroupBy::Name,
        Some("kind") => smeltr_analyzer::OpGroupBy::Kind,
        Some(other) => {
            return Err(ToolError::BadArgs(format!(
                "group_by must be \"name\" or \"kind\", got {other:?}"
            )))
        }
    };

    let dir = resolve_session(&params.session)?;
    let events = read_events(&dir)?;
    let root =
        compute_breakdown(events).map_err(|e| ToolError::BadArgs(format!("breakdown: {e}")))?;

    let top = params.top_n.unwrap_or(10) as usize;
    let rows = smeltr_analyzer::aggregate_ops_flat(&root, group_by);
    let total: u64 = rows.iter().map(|r| r.gpu_ns).sum::<u64>().max(1);
    let mut ops: Vec<OpSummary> = rows
        .into_iter()
        .map(|r| OpSummary {
            name: r.key,
            gpu_ns: r.gpu_ns,
            count: r.count,
            pct: (r.gpu_ns as f64 / total as f64) * 100.0,
            symbol: r.symbol,
            kind: r.kind,
        })
        .collect();
    ops.truncate(top);
    Ok(Response { ops })
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
    fn returns_sorted_ops() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        let evs: Vec<Event> = vec![
            Event {
                ts_mono_ns: 1,
                ts_wall_ns: 1,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 1,
                payload: Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 1,
                    qualname: "A".into(),
                    class_name: "A".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields: Default::default(),
                },
            },
            Event {
                ts_mono_ns: 10,
                ts_wall_ns: 10,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 2,
                payload: Payload::MlxEvalEntered {
                    call_id: 1,
                    array_count: 1,
                    stream: "gpu".into(),
                    module_stack: vec![1],
                    stack_frames: vec![],
                },
            },
            Event {
                ts_mono_ns: 20,
                ts_wall_ns: 20,
                session_id: Uuid::nil(),
                source: Source::MetalHook,
                pid: None,
                seq: 3,
                payload: Payload::MetalCbCommitted {
                    cb_id: 9,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
            },
            Event {
                ts_mono_ns: 30,
                ts_wall_ns: 30,
                session_id: Uuid::nil(),
                source: Source::MetalHook,
                pid: None,
                seq: 4,
                payload: Payload::MetalCbCompleted {
                    cb_id: 9,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 200,
                },
            },
            Event {
                ts_mono_ns: 31,
                ts_wall_ns: 31,
                session_id: Uuid::nil(),
                source: Source::MetalHook,
                pid: None,
                seq: 5,
                payload: Payload::MetalCbOps {
                    cb_id: 9,
                    ops: vec![
                        smeltr_core::event::OpSample {
                            name: "Matmul".into(),
                            symbol: None,
                            gpu_ns: 150,
                            count: 1,
                        },
                        smeltr_core::event::OpSample {
                            name: "Softmax".into(),
                            symbol: None,
                            gpu_ns: 50,
                            count: 2,
                        },
                    ],
                },
            },
            Event {
                ts_mono_ns: 40,
                ts_wall_ns: 40,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 6,
                payload: Payload::MlxEvalReturned {
                    call_id: 1,
                    duration_ns: 30,
                    was_async: false,
                },
            },
            Event {
                ts_mono_ns: 50,
                ts_wall_ns: 50,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 7,
                payload: Payload::ModuleReturned { module_call_id: 1 },
            },
        ];
        for e in &evs {
            w.write_event(e).unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
            top_n: None,
            group_by: None,
        })
        .unwrap();
        assert_eq!(resp.ops.len(), 2);
        assert_eq!(resp.ops[0].name, "Matmul"); // gpu_ns 150 > 50, sorted desc
        assert_eq!(resp.ops[0].count, 1);
        assert_eq!(resp.ops[1].name, "Softmax");
        assert!(resp.ops[0].pct > resp.ops[1].pct);
    }

    #[test]
    #[serial_test::serial]
    fn group_by_kind_collapses_matmuls() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        // Two distinct op names with gemm_* symbols → both resolve to Matmul kind.
        let evs: Vec<Event> = vec![
            Event {
                ts_mono_ns: 1,
                ts_wall_ns: 1,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 1,
                payload: Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 1,
                    qualname: "A".into(),
                    class_name: "A".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields: Default::default(),
                },
            },
            Event {
                ts_mono_ns: 10,
                ts_wall_ns: 10,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 2,
                payload: Payload::MlxEvalEntered {
                    call_id: 1,
                    array_count: 1,
                    stream: "gpu".into(),
                    module_stack: vec![1],
                    stack_frames: vec![],
                },
            },
            Event {
                ts_mono_ns: 20,
                ts_wall_ns: 20,
                session_id: Uuid::nil(),
                source: Source::MetalHook,
                pid: None,
                seq: 3,
                payload: Payload::MetalCbCommitted {
                    cb_id: 9,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
            },
            Event {
                ts_mono_ns: 30,
                ts_wall_ns: 30,
                session_id: Uuid::nil(),
                source: Source::MetalHook,
                pid: None,
                seq: 4,
                payload: Payload::MetalCbCompleted {
                    cb_id: 9,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 200,
                },
            },
            Event {
                ts_mono_ns: 31,
                ts_wall_ns: 31,
                session_id: Uuid::nil(),
                source: Source::MetalHook,
                pid: None,
                seq: 5,
                payload: Payload::MetalCbOps {
                    cb_id: 9,
                    ops: vec![
                        smeltr_core::event::OpSample {
                            name: "K_gemm_1".into(),
                            symbol: Some("gemm_nn_batchedstrided_128x128x8_2stage".into()),
                            gpu_ns: 100,
                            count: 1,
                        },
                        smeltr_core::event::OpSample {
                            name: "K_gemm_2".into(),
                            symbol: Some("gemm_tn_batchedstrided_128x64x8_2stage".into()),
                            gpu_ns: 80,
                            count: 1,
                        },
                    ],
                },
            },
            Event {
                ts_mono_ns: 40,
                ts_wall_ns: 40,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 6,
                payload: Payload::MlxEvalReturned {
                    call_id: 1,
                    duration_ns: 30,
                    was_async: false,
                },
            },
            Event {
                ts_mono_ns: 50,
                ts_wall_ns: 50,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 7,
                payload: Payload::ModuleReturned { module_call_id: 1 },
            },
        ];
        for e in &evs {
            w.write_event(e).unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
            group_by: Some("kind".into()),
            top_n: None,
        })
        .unwrap();
        // Both gemm ops collapse into a single "Matmul" kind row.
        assert!(
            resp.ops.iter().any(|o| o.name == "Matmul"),
            "expected a Matmul row; got {:?}",
            resp.ops.iter().map(|o| &o.name).collect::<Vec<_>>()
        );
        let matmul = resp.ops.iter().find(|o| o.name == "Matmul").unwrap();
        // Kind mode: no representative symbol.
        assert!(matmul.symbol.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn unknown_group_by_is_bad_args() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let r = run(Params {
            session: "x".into(),
            group_by: Some("nope".into()),
            top_n: None,
        });
        assert!(matches!(r, Err(ToolError::BadArgs(_))));
    }

    #[test]
    #[serial_test::serial]
    fn op_with_symbol_gets_kind_resolved() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        let evs: Vec<Event> = vec![
            Event {
                ts_mono_ns: 1,
                ts_wall_ns: 1,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 1,
                payload: Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 1,
                    qualname: "A".into(),
                    class_name: "A".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields: Default::default(),
                },
            },
            Event {
                ts_mono_ns: 10,
                ts_wall_ns: 10,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 2,
                payload: Payload::MlxEvalEntered {
                    call_id: 1,
                    array_count: 1,
                    stream: "gpu".into(),
                    module_stack: vec![1],
                    stack_frames: vec![],
                },
            },
            Event {
                ts_mono_ns: 20,
                ts_wall_ns: 20,
                session_id: Uuid::nil(),
                source: Source::MetalHook,
                pid: None,
                seq: 3,
                payload: Payload::MetalCbCommitted {
                    cb_id: 9,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
            },
            Event {
                ts_mono_ns: 30,
                ts_wall_ns: 30,
                session_id: Uuid::nil(),
                source: Source::MetalHook,
                pid: None,
                seq: 4,
                payload: Payload::MetalCbCompleted {
                    cb_id: 9,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 200,
                },
            },
            Event {
                ts_mono_ns: 31,
                ts_wall_ns: 31,
                session_id: Uuid::nil(),
                source: Source::MetalHook,
                pid: None,
                seq: 5,
                payload: Payload::MetalCbOps {
                    cb_id: 9,
                    ops: vec![smeltr_core::event::OpSample {
                        name: "K_attn_1".into(),
                        symbol: Some("sdpa_vector_2pass_1_float16_64".into()),
                        gpu_ns: 200,
                        count: 1,
                    }],
                },
            },
            Event {
                ts_mono_ns: 40,
                ts_wall_ns: 40,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 6,
                payload: Payload::MlxEvalReturned {
                    call_id: 1,
                    duration_ns: 30,
                    was_async: false,
                },
            },
            Event {
                ts_mono_ns: 50,
                ts_wall_ns: 50,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 7,
                payload: Payload::ModuleReturned { module_call_id: 1 },
            },
        ];
        for e in &evs {
            w.write_event(e).unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
            top_n: None,
            group_by: None,
        })
        .unwrap();
        let row = resp
            .ops
            .iter()
            .find(|r| r.name == "K_attn_1")
            .expect("row present");
        assert_eq!(
            row.symbol.as_deref(),
            Some("sdpa_vector_2pass_1_float16_64")
        );
        assert_eq!(row.kind.as_deref(), Some("ScaledDotProductAttention"));
    }
}

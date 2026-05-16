//! `get_op_summary` MCP tool: flat cross-module aggregation of GPU ops.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_analyzer::{compute_breakdown, ModuleBreakdown};
use smeltr_core::reader::read_events;
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct Params {
    pub session: String,
    pub top_n: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct OpSummary {
    pub name: String,
    pub gpu_ns: u64,
    pub count: u64,
    pub pct: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ops: Vec<OpSummary>,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let dir = resolve_session(&params.session)?;
    let events = read_events(&dir)?;
    let root =
        compute_breakdown(events).map_err(|e| ToolError::BadArgs(format!("breakdown: {e}")))?;

    let top = params.top_n.unwrap_or(10) as usize;
    let mut agg: HashMap<String, (u64, u64)> = HashMap::new();
    fn walk(n: &ModuleBreakdown, agg: &mut HashMap<String, (u64, u64)>) {
        for op in &n.ops {
            let e = agg.entry(op.name.clone()).or_insert((0, 0));
            e.0 += op.gpu_ns;
            e.1 += op.count;
        }
        for c in &n.children {
            walk(c, agg);
        }
    }
    walk(&root, &mut agg);

    let total: u64 = agg.values().map(|(ns, _)| *ns).sum::<u64>().max(1);
    let mut rows: Vec<OpSummary> = agg
        .into_iter()
        .map(|(name, (gpu_ns, count))| OpSummary {
            name,
            gpu_ns,
            count,
            pct: (gpu_ns as f64 / total as f64) * 100.0,
        })
        .collect();
    rows.sort_by_key(|r| std::cmp::Reverse(r.gpu_ns));
    rows.truncate(top);
    Ok(Response { ops: rows })
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
                            gpu_ns: 150,
                            count: 1,
                        },
                        smeltr_core::event::OpSample {
                            name: "Softmax".into(),
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
        })
        .unwrap();
        assert_eq!(resp.ops.len(), 2);
        assert_eq!(resp.ops[0].name, "Matmul"); // gpu_ns 150 > 50, sorted desc
        assert_eq!(resp.ops[0].count, 1);
        assert_eq!(resp.ops[1].name, "Softmax");
        assert!(resp.ops[0].pct > resp.ops[1].pct);
    }
}

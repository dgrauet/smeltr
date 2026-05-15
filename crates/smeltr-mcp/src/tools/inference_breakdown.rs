//! `get_inference_breakdown` MCP tool.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_analyzer::{compute_breakdown, ModuleBreakdown};
use smeltr_core::reader::read_events;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct Params {
    pub session: String,
    pub max_depth: Option<u16>,
    pub top_n: Option<u32>,
    pub min_gpu_ns: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub root: ModuleBreakdown,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let dir = resolve_session(&params.session)?;
    let events = read_events(&dir)?;
    let mut root =
        compute_breakdown(events).map_err(|e| ToolError::BadArgs(format!("breakdown: {e}")))?;

    let max_depth = params.max_depth.unwrap_or(u16::MAX);
    let top_n = params.top_n.unwrap_or(u32::MAX) as usize;
    let min_gpu_ns = params.min_gpu_ns.unwrap_or(0);

    fn prune(n: &mut ModuleBreakdown, depth: u16, max_depth: u16, top_n: usize, min_gpu_ns: u64) {
        if depth >= max_depth {
            n.children.clear();
            return;
        }
        n.children.retain(|c| c.gpu_ns_subtree >= min_gpu_ns);
        n.children
            .sort_by_key(|c| std::cmp::Reverse(c.gpu_ns_subtree));
        if n.children.len() > top_n {
            n.children.truncate(top_n);
        }
        for c in &mut n.children {
            prune(c, depth + 1, max_depth, top_n, min_gpu_ns);
        }
    }
    prune(&mut root, 0, max_depth, top_n, min_gpu_ns);

    Ok(Response { root })
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
    fn returns_tree_with_pruning() {
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
                    in_flight_ns: 100,
                },
            },
            Event {
                ts_mono_ns: 40,
                ts_wall_ns: 40,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 5,
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
                seq: 6,
                payload: Payload::ModuleReturned { module_call_id: 1 },
            },
        ];
        for e in &evs {
            w.write_event(e).unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
            min_gpu_ns: Some(50),
            ..Default::default()
        })
        .unwrap();
        assert!(resp.root.children.iter().any(|c| c.qualname == "A"));
    }
}

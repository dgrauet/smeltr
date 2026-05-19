//! `get_memory_breakdown` MCP tool: per-scope device + heap memory.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_analyzer::memory::{
    compute_heap_breakdown, compute_memory_breakdown, HeapMemory, ScopeMemory,
};
use smeltr_core::reader::read_events;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    pub session: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub scope_memory: Vec<ScopeMemory>,
    pub heap_memory: Vec<HeapMemory>,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let dir = resolve_session(&params.session)?;
    let events = read_events(&dir)?;
    Ok(Response {
        scope_memory: compute_memory_breakdown(&events),
        heap_memory: compute_heap_breakdown(&events),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Event, Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

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

    #[test]
    #[serial_test::serial]
    fn memory_breakdown_returns_scope_and_heap() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
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
                },
            ),
            ev(
                2,
                2,
                Source::MetalHook,
                Payload::MetalDeviceMemSample {
                    allocated_bytes: 1_000_000,
                    recommended_max_bytes: 4_000_000,
                    at_event: "cb_committed".into(),
                },
            ),
            ev(
                3,
                3,
                Source::MetalHook,
                Payload::MetalHeapAlloc {
                    heap_id: 7,
                    size_bytes: 500_000,
                    label: None,
                },
            ),
            ev(
                4,
                4,
                Source::PythonSidecar,
                Payload::ModuleReturned { module_call_id: 1 },
            ),
        ];
        for e in &evs {
            w.write_event(e).unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
        })
        .unwrap();
        let scope = resp
            .scope_memory
            .iter()
            .find(|s| s.qualname == "denoise.pass:cond")
            .expect("scope present");
        assert_eq!(scope.peak_bytes, 1_000_000);

        let heap = resp
            .heap_memory
            .iter()
            .find(|h| h.qualname == "denoise.pass:cond")
            .expect("heap present");
        assert_eq!(heap.peak_heap_count, 1);
        assert_eq!(heap.peak_heap_bytes, 500_000);
    }
}

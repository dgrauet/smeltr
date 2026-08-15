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
    /// Include a time-resolved profile (#182): per-bucket peaks + distinct
    /// over-budget windows. Off by default (payload size).
    #[serde(default)]
    pub include_timeline: bool,
    /// Bucket width in seconds for the timeline (default 10).
    #[serde(default = "default_bucket_seconds")]
    pub bucket_seconds: u64,
}

fn default_bucket_seconds() -> u64 {
    10
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub scope_memory: Vec<ScopeMemory>,
    pub heap_memory: Vec<HeapMemory>,
    /// Present when `include_timeline` was requested (#182).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeline: Option<smeltr_analyzer::memory::MemTimeline>,
    /// Pourquoi les ventilations sont vides, quand elles le sont. Absent du
    /// JSON dès qu'il y a quelque chose à ventiler.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let dir = resolve_session(&params.session)?;
    let events = read_events(&dir)?;
    let timeline = params
        .include_timeline
        .then(|| smeltr_analyzer::memory::compute_memory_timeline(&events, params.bucket_seconds));
    let scope_memory = compute_memory_breakdown(&events);
    let heap_memory = compute_heap_breakdown(&events);

    let mut notes = Vec::new();
    if scope_memory.is_empty() && heap_memory.is_empty() {
        use smeltr_analyzer::rules::sidecar_absent::{detect, detect_nothing_instrumented};
        if let Some(nothing) = detect_nothing_instrumented(&events) {
            notes.push(nothing.advice());
        } else if let Some(absent) = detect(&events) {
            notes.push(absent.advice());
        }
    }

    Ok(Response {
        scope_memory,
        heap_memory,
        timeline,
        notes,
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
                    fields: Default::default(),
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
            include_timeline: false,
            bucket_seconds: 10,
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

    #[test]
    #[serial_test::serial]
    fn empty_breakdown_explains_why() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        // Session ambiante : que des sondes système.
        w.write_event(&ev(
            1,
            1,
            Source::Vm,
            Payload::VmSample {
                wired_bytes: 1,
                active_bytes: 1,
                compressed_bytes: 0,
                swap_used_bytes: 0,
                page_outs_per_sec: 0.0,
            },
        ))
        .unwrap();
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params {
            include_timeline: false,
            bucket_seconds: 10,
            session: id.short(),
        })
        .unwrap();

        assert!(resp.scope_memory.is_empty());
        assert!(resp.heap_memory.is_empty());
        assert_eq!(resp.notes.len(), 1, "notes: {:?}", resp.notes);
        assert!(resp.notes[0].contains("instrument"));
    }

    #[test]
    #[serial_test::serial]
    fn instrumented_breakdown_has_no_notes() {
        // Réutilise la session du test memory_breakdown_returns_scope_and_heap :
        // dès qu'il y a des scopes, il n'y a rien à expliquer.
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        for e in [
            ev(
                1,
                1,
                Source::PythonSidecar,
                Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 0,
                    qualname: "denoise".into(),
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
                    allocated_bytes: 1_000,
                    recommended_max_bytes: 4_000,
                    at_event: "cb_committed".into(),
                },
            ),
            ev(
                3,
                3,
                Source::PythonSidecar,
                Payload::ModuleReturned { module_call_id: 1 },
            ),
        ] {
            w.write_event(&e).unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params {
            include_timeline: false,
            bucket_seconds: 10,
            session: id.short(),
        })
        .unwrap();
        assert!(resp.notes.is_empty(), "notes: {:?}", resp.notes);
    }

    /// `notes` vide ne doit pas apparaître du tout dans le JSON : la sortie
    /// des sessions instrumentées ne change pas d'un octet.
    #[test]
    fn empty_notes_are_omitted_from_json() {
        let resp = Response {
            scope_memory: vec![],
            heap_memory: vec![],
            timeline: None,
            notes: vec![],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("notes"), "json: {json}");
    }
}

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
    /// Why the breakdowns are empty, when they are. Absent from the JSON as
    /// soon as there is anything to break down.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Per-process memory footprint over the traced tree — the metric jetsam
    /// decides on. Absent when the probe recorded nothing (session predating
    /// the probe, or an unscoped run).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub process_footprint: Vec<smeltr_analyzer::footprint::ProcFootprintSummary>,
    /// The MLX allocator's view — active, peak, and above all the cache:
    /// buffers MLX retains after they were freed. Indistinguishable from
    /// working memory when seen from Metal, hence their presence next to the
    /// MTLDevice figures. Absent when the sidecar emitted nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mlx_allocator: Option<smeltr_analyzer::memory::MlxAllocator>,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let dir = resolve_session(&params.session)?;
    let events = read_events(&dir)?;
    let timeline = params
        .include_timeline
        .then(|| smeltr_analyzer::memory::compute_memory_timeline(&events, params.bucket_seconds));
    let mlx_allocator = smeltr_analyzer::memory::compute_mlx_allocator(&events);
    let scope_memory = compute_memory_breakdown(&events);
    let heap_memory = compute_heap_breakdown(&events);
    let process_footprint = smeltr_analyzer::footprint::compute_footprint_summary(&events);

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
        process_footprint,
        mlx_allocator,
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
        // Ambient session: system probes only.
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
        // Reuses the session from memory_breakdown_returns_scope_and_heap: as
        // soon as scopes exist there is nothing to explain.
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

    /// An empty `notes` must not appear in the JSON at all: the output of
    /// instrumented sessions does not change by a single byte.
    #[test]
    fn empty_notes_are_omitted_from_json() {
        let resp = Response {
            scope_memory: vec![],
            heap_memory: vec![],
            timeline: None,
            notes: vec![],
            process_footprint: vec![],
            mlx_allocator: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("notes"), "json: {json}");
    }

    /// The MLX field ships in the response BY DEFAULT, behind no flag: a
    /// number hidden behind an option that is off by default is exactly what
    /// kept ltx-2-mlx#79 invisible for its entire lifetime.
    #[test]
    #[serial_test::serial]
    fn mlx_allocator_is_in_the_default_response() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        w.write_event(&ev(
            1,
            1,
            Source::PythonSidecar,
            Payload::MlxMemoryPoll {
                active_bytes: 15_250_000_000,
                peak_bytes: 15_250_000_000,
                cache_bytes: 21_310_000_000,
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

        let a = resp.mlx_allocator.expect("l'allocateur MLX doit remonter");
        assert_eq!(a.peak_cache_bytes, 21_310_000_000);
        assert_eq!(a.peak_active_bytes, 15_250_000_000);
        assert_eq!(a.sample_count, 1);
    }

    /// With no MLX samples the field vanishes from the JSON: the output of
    /// uninstrumented sessions does not change by a single byte.
    #[test]
    fn absent_mlx_allocator_is_omitted_from_json() {
        let resp = Response {
            scope_memory: vec![],
            heap_memory: vec![],
            timeline: None,
            notes: vec![],
            process_footprint: vec![],
            mlx_allocator: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("mlx_allocator"), "json: {json}");
    }

    #[test]
    fn empty_process_footprint_is_omitted_from_json() {
        let resp = Response {
            scope_memory: vec![],
            heap_memory: vec![],
            timeline: None,
            notes: vec![],
            process_footprint: vec![],
            mlx_allocator: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("process_footprint"), "json: {json}");
    }
}

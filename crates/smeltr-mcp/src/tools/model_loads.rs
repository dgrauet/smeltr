//! `get_model_loads` MCP tool: list model loads with duplicate detection.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_core::event::{Event, Payload};
use smeltr_core::reader::read_events;
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    /// Session ref: short id (8 hex), full UUID, or session name.
    pub session: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ModelLoadInfo {
    pub path: String,
    pub size_bytes: u64,
    pub t_start_ns: u64,
    pub t_end_ns: u64,
    pub duration_ns: u64,
    pub sha8: Option<String>,
    pub framework: Option<String>,
    /// Index of the first load with the same canonical path. None if this is the first.
    pub duplicate_of: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Response {
    pub session_id: String,
    pub session_name: Option<String>,
    pub loads: Vec<ModelLoadInfo>,
    /// Count of entries where `duplicate_of` is Some.
    pub duplicate_count: usize,
}

/// Pure, testable logic: extract model loads and mark duplicates.
pub fn compute_model_loads(events: &[Event]) -> Vec<ModelLoadInfo> {
    let mut loads: Vec<ModelLoadInfo> = Vec::new();
    // Map from canonical path to first index in `loads`.
    let mut first_seen: HashMap<String, usize> = HashMap::new();

    for ev in events {
        if let Payload::ModelLoad {
            path,
            size_bytes,
            t_start_ns,
            t_end_ns,
            sha8,
            framework,
        } = &ev.payload
        {
            let idx = loads.len();
            let duplicate_of = if let Some(&first) = first_seen.get(path.as_str()) {
                Some(first)
            } else {
                first_seen.insert(path.clone(), idx);
                None
            };
            loads.push(ModelLoadInfo {
                path: path.clone(),
                size_bytes: *size_bytes,
                t_start_ns: *t_start_ns,
                t_end_ns: *t_end_ns,
                duration_ns: t_end_ns - t_start_ns,
                sha8: sha8.clone(),
                framework: framework.clone(),
                duplicate_of,
            });
        }
    }

    loads
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let dir = resolve_session(&params.session)?;
    let meta = smeltr_core::reader::read_metadata(&dir).ok();
    let events = read_events(&dir)?;
    let loads = compute_model_loads(&events);
    let duplicate_count = loads.iter().filter(|l| l.duplicate_of.is_some()).count();
    Ok(Response {
        session_id: dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string(),
        session_name: meta.and_then(|m| m.name),
        loads,
        duplicate_count,
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

    fn model_load_ev(seq: u64, path: &str, size: u64) -> Event {
        ev(
            seq,
            seq * 1000,
            Source::PythonSidecar,
            Payload::ModelLoad {
                path: path.to_string(),
                size_bytes: size,
                t_start_ns: seq * 1000,
                t_end_ns: seq * 1000 + 500,
                sha8: None,
                framework: None,
            },
        )
    }

    #[test]
    fn compute_model_loads_no_loads_returns_empty() {
        let evs: Vec<Event> = vec![];
        let result = compute_model_loads(&evs);
        assert!(result.is_empty());
    }

    #[test]
    fn compute_model_loads_single_load_no_duplicate() {
        let evs = vec![model_load_ev(1, "/models/weights.safetensors", 1024)];
        let result = compute_model_loads(&evs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].path, "/models/weights.safetensors");
        assert_eq!(result[0].size_bytes, 1024);
        assert_eq!(result[0].duration_ns, 500);
        assert!(result[0].duplicate_of.is_none());
    }

    #[test]
    fn compute_model_loads_three_same_path_marks_duplicates() {
        let evs = vec![
            model_load_ev(1, "/models/weights.safetensors", 1024),
            model_load_ev(2, "/models/weights.safetensors", 1024),
            model_load_ev(3, "/models/weights.safetensors", 1024),
        ];
        let result = compute_model_loads(&evs);
        assert_eq!(result.len(), 3);
        // First is not a duplicate
        assert!(result[0].duplicate_of.is_none());
        // Second and third are duplicates of the first (index 0)
        assert_eq!(result[1].duplicate_of, Some(0));
        assert_eq!(result[2].duplicate_of, Some(0));
    }

    #[test]
    fn compute_model_loads_duplicate_count_is_two_for_three_same_path() {
        let evs = vec![
            model_load_ev(1, "/models/weights.safetensors", 1024),
            model_load_ev(2, "/models/weights.safetensors", 1024),
            model_load_ev(3, "/models/weights.safetensors", 1024),
        ];
        let result = compute_model_loads(&evs);
        let dup_count = result.iter().filter(|l| l.duplicate_of.is_some()).count();
        assert_eq!(dup_count, 2);
    }

    #[test]
    fn compute_model_loads_distinct_paths_have_no_duplicates() {
        let evs = vec![
            model_load_ev(1, "/models/llama/weights.safetensors", 1024),
            model_load_ev(2, "/models/mistral/model.safetensors", 2048),
        ];
        let result = compute_model_loads(&evs);
        assert_eq!(result.len(), 2);
        assert!(result[0].duplicate_of.is_none());
        assert!(result[1].duplicate_of.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn run_returns_correct_session_id_and_loads() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        let evs = vec![
            model_load_ev(1, "/models/a.safetensors", 100),
            model_load_ev(2, "/models/a.safetensors", 100),
        ];
        for e in &evs {
            w.write_event(e).unwrap();
        }
        w.finalize(Some(0), "ok".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
        })
        .unwrap();
        assert_eq!(resp.loads.len(), 2);
        assert_eq!(resp.duplicate_count, 1);
        assert!(resp.loads[0].duplicate_of.is_none());
        assert_eq!(resp.loads[1].duplicate_of, Some(0));
    }
}

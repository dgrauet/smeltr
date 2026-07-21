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
    /// Index of the previous load with the same canonical path (chained: dup2 → dup1 → original).
    /// None if this is the first load (or first after an unload).
    pub duplicate_of: Option<usize>,
    /// Monotonic ns when the matching ModelUnload fired. None if still loaded (or unload not seen).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unloaded_at_ns: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Response {
    pub session_id: String,
    pub session_name: Option<String>,
    pub loads: Vec<ModelLoadInfo>,
    /// Count of entries where `duplicate_of` is Some.
    pub duplicate_count: usize,
}

/// Pure, testable logic: extract model loads, mark duplicates, and record unload timestamps.
///
/// Chronological walk:
/// - `ModelLoad`: if the path is currently tracked, the new load is `duplicate_of`
///   the previous load index; update the current index. Otherwise first load.
/// - `ModelUnload`: look up the current load index for that path, set its
///   `unloaded_at_ns`, remove from the map.
pub fn compute_model_loads(events: &[Event]) -> Vec<ModelLoadInfo> {
    let mut loads: Vec<ModelLoadInfo> = Vec::new();
    // Map from canonical path to index of the most recent load in `loads`.
    let mut current_load_idx: HashMap<String, usize> = HashMap::new();

    for ev in events {
        match &ev.payload {
            Payload::ModelLoad {
                path,
                size_bytes,
                t_start_ns,
                t_end_ns,
                sha8,
                framework,
            } => {
                let idx = loads.len();
                let duplicate_of = if let Some(&prev_idx) = current_load_idx.get(path.as_str()) {
                    Some(prev_idx)
                } else {
                    None
                };
                // Update current load index for this path (whether or not it's a dup).
                current_load_idx.insert(path.clone(), idx);
                // Payload t_*_ns are client-clock (uptime base); only the
                // duration is meaningful. Anchor the load end at the event's
                // session-relative ingest timestamp so reported times can be
                // correlated with other event streams.
                let duration_ns = t_end_ns.saturating_sub(*t_start_ns);
                loads.push(ModelLoadInfo {
                    path: path.clone(),
                    size_bytes: *size_bytes,
                    t_start_ns: ev.ts_mono_ns.saturating_sub(duration_ns),
                    t_end_ns: ev.ts_mono_ns,
                    duration_ns,
                    sha8: sha8.clone(),
                    framework: framework.clone(),
                    duplicate_of,
                    unloaded_at_ns: None,
                });
            }
            Payload::ModelUnload { path, .. } => {
                if let Some(&idx) = current_load_idx.get(path.as_str()) {
                    loads[idx].unloaded_at_ns = Some(ev.ts_mono_ns);
                    current_load_idx.remove(path.as_str());
                }
            }
            _ => {}
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

    fn model_unload_ev(seq: u64, path: &str) -> Event {
        ev(
            seq,
            seq * 1000,
            Source::PythonSidecar,
            Payload::ModelUnload {
                path: path.to_string(),
                t_ns: seq * 1000,
                sha8: None,
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
        assert!(result[0].unloaded_at_ns.is_none());
    }

    #[test]
    fn compute_model_loads_rebases_payload_clock_to_session_ts() {
        // Payload t_*_ns come from client time.monotonic_ns() (uptime base);
        // reported times must be session-relative (anchored on ev.ts_mono_ns)
        // so consumers can correlate with query_events timestamps.
        let uptime = 627_000_000_000_000_u64;
        let evs = vec![
            ev(
                1,
                5_000_000,
                Source::PythonSidecar,
                Payload::ModelLoad {
                    path: "/models/weights.safetensors".to_string(),
                    size_bytes: 1024,
                    t_start_ns: uptime + 1_000_000,
                    t_end_ns: uptime + 3_000_000,
                    sha8: None,
                    framework: None,
                },
            ),
            ev(
                2,
                9_000_000,
                Source::PythonSidecar,
                Payload::ModelUnload {
                    path: "/models/weights.safetensors".to_string(),
                    t_ns: uptime + 8_000_000,
                    sha8: None,
                },
            ),
        ];
        let result = compute_model_loads(&evs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].duration_ns, 2_000_000);
        assert_eq!(result[0].t_end_ns, 5_000_000, "load end = ingest ts");
        assert_eq!(result[0].t_start_ns, 3_000_000, "start = ingest - dur");
        assert_eq!(result[0].unloaded_at_ns, Some(9_000_000));
    }

    #[test]
    fn compute_model_loads_three_same_path_chained_dup_refs() {
        // Three loads without unloads: dup2 → dup1 → original (chained, not always → 0).
        let evs = vec![
            model_load_ev(1, "/models/weights.safetensors", 1024),
            model_load_ev(2, "/models/weights.safetensors", 1024),
            model_load_ev(3, "/models/weights.safetensors", 1024),
        ];
        let result = compute_model_loads(&evs);
        assert_eq!(result.len(), 3);
        // First is not a duplicate.
        assert!(result[0].duplicate_of.is_none());
        // Second duplicates the first (index 0).
        assert_eq!(result[1].duplicate_of, Some(0));
        // Third duplicates the second (index 1) — chained, not always → 0.
        assert_eq!(result[2].duplicate_of, Some(1));
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
    fn compute_model_loads_unload_sets_unloaded_at_ns() {
        let evs = vec![
            model_load_ev(1, "/models/a.safetensors", 1024),
            model_unload_ev(2, "/models/a.safetensors"),
        ];
        let result = compute_model_loads(&evs);
        assert_eq!(result.len(), 1);
        assert!(result[0].duplicate_of.is_none());
        assert_eq!(result[0].unloaded_at_ns, Some(2 * 1000));
    }

    #[test]
    fn compute_model_loads_load_unload_reload_no_dup_second_no_unloaded_at() {
        // load → unload → reload: no dup, first has unloaded_at_ns, second does not.
        let evs = vec![
            model_load_ev(1, "/models/a.safetensors", 1024),
            model_unload_ev(2, "/models/a.safetensors"),
            model_load_ev(3, "/models/a.safetensors", 1024),
        ];
        let result = compute_model_loads(&evs);
        assert_eq!(result.len(), 2);
        // First load was unloaded.
        assert!(result[0].duplicate_of.is_none());
        assert_eq!(result[0].unloaded_at_ns, Some(2 * 1000));
        // Second load is not a duplicate (unload cleared the map).
        assert!(result[1].duplicate_of.is_none());
        assert!(result[1].unloaded_at_ns.is_none());
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
        // Second duplicates first (index 0) — chained.
        assert_eq!(resp.loads[1].duplicate_of, Some(0));
    }
}

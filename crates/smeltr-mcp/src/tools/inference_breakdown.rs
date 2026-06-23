//! `get_inference_breakdown` MCP tool.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_analyzer::{apply_op_group_by, compute_breakdown, ModuleBreakdown, OpGroupBy};
use smeltr_core::event::FieldValue;
use smeltr_core::reader::read_events;
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    pub session: String,
    pub max_depth: Option<u16>,
    pub top_n: Option<u32>,
    pub min_gpu_ns: Option<u64>,
    #[serde(default = "default_include_ops")]
    pub include_ops: bool,
    #[serde(default = "default_top_ops")]
    pub top_ops_per_leaf: u32,
    /// Exact-match field filter. Keys are field names; values are JSON
    /// scalars (bool, integer, float, or string). A node is kept if its
    /// `fields` map contains all specified key/value pairs (superset
    /// match). Ancestors of matching nodes are also retained. Empty or
    /// absent = no filtering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_filter: Option<BTreeMap<String, serde_json::Value>>,
    /// How to group ops on each leaf node. `"name"` (default) keeps each
    /// distinct op name as its own row. `"kind"` collapses ops that share
    /// the same resolved kind (e.g. `"Matmul"`) into a single row with
    /// summed `gpu_ns`/`count` and `symbol` set to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_by: Option<String>,
}

fn default_include_ops() -> bool {
    true
}
fn default_top_ops() -> u32 {
    5
}

impl Default for Params {
    fn default() -> Self {
        Self {
            session: String::new(),
            max_depth: None,
            top_n: None,
            min_gpu_ns: None,
            include_ops: default_include_ops(),
            top_ops_per_leaf: default_top_ops(),
            field_filter: None,
            group_by: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub root: ModuleBreakdown,
}

/// Recursively prunes nodes whose own fields don't match the filter
/// AND none of their descendants match either. A node matches if its
/// `fields` map is a superset of `filter` (every key/value in `filter`
/// is present in `fields` with equal value).
///
/// Returns true if the node itself (or any descendant) matches.
fn prune_by_field_filter(
    node: &mut ModuleBreakdown,
    filter: &BTreeMap<String, FieldValue>,
) -> bool {
    // Recurse first so child match info is up-to-date.
    let mut any_child_matches = false;
    let mut kept = Vec::with_capacity(node.children.len());
    for mut child in std::mem::take(&mut node.children) {
        if prune_by_field_filter(&mut child, filter) {
            any_child_matches = true;
            kept.push(child);
        }
    }
    node.children = kept;

    let self_matches = filter.iter().all(|(k, v)| node.fields.get(k) == Some(v));
    self_matches || any_child_matches
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    // Validate group_by early so callers get BadArgs before any I/O.
    let group_by = match params.group_by.as_deref() {
        None | Some("name") => OpGroupBy::Name,
        Some("kind") => OpGroupBy::Kind,
        Some(other) => {
            return Err(ToolError::BadArgs(format!(
                "group_by must be \"name\" or \"kind\", got {other:?}"
            )))
        }
    };

    let dir = resolve_session(&params.session)?;
    let events = read_events(&dir)?;
    let mut root =
        compute_breakdown(events).map_err(|e| ToolError::BadArgs(format!("breakdown: {e}")))?;

    if let Some(raw_filter) = params.field_filter.as_ref() {
        if !raw_filter.is_empty() {
            // Convert JSON values → FieldValue. Unknown shapes are skipped
            // (treated as no-match, which is the safe default).
            let filter: BTreeMap<String, FieldValue> = raw_filter
                .iter()
                .filter_map(|(k, v)| {
                    let fv = serde_json::from_value::<FieldValue>(v.clone()).ok()?;
                    Some((k.clone(), fv))
                })
                .collect();
            if !filter.is_empty() {
                prune_by_field_filter(&mut root, &filter);
            }
        }
    }

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

    apply_op_group_by(&mut root, group_by);

    let top_ops_per_leaf = params.top_ops_per_leaf as usize;
    let include_ops = params.include_ops;
    fn shape_ops(n: &mut ModuleBreakdown, include: bool, top: usize) {
        if !include {
            n.ops.clear();
        } else if n.ops.len() > top {
            n.ops.truncate(top);
        }
        for c in &mut n.children {
            shape_ops(c, include, top);
        }
    }
    shape_ops(&mut root, include_ops, top_ops_per_leaf);

    Ok(Response { root })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Event, FieldValue, Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    /// Helper: convert a FieldValue into a serde_json::Value for use in
    /// Params::field_filter (which stores JSON scalars to avoid a schemars dep).
    fn fv_to_json(v: &FieldValue) -> serde_json::Value {
        serde_json::to_value(v).unwrap()
    }

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

    #[test]
    #[serial_test::serial]
    fn include_ops_false_strips_ops() {
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
                    in_flight_ns: 100,
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
                        name: "Matmul".into(),
                        symbol: None,
                        gpu_ns: 50,
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

        // include_ops=true (default) → ops present
        let resp1 = run(Params {
            session: id.short(),
            ..Default::default()
        })
        .unwrap();
        let a1 = resp1
            .root
            .children
            .iter()
            .find(|c| c.qualname == "A")
            .unwrap();
        assert!(!a1.ops.is_empty(), "default should keep ops");

        // include_ops=false → ops stripped
        let resp2 = run(Params {
            session: id.short(),
            include_ops: false,
            top_ops_per_leaf: 5,
            ..Default::default()
        })
        .unwrap();
        let a2 = resp2
            .root
            .children
            .iter()
            .find(|c| c.qualname == "A")
            .unwrap();
        assert!(a2.ops.is_empty(), "include_ops=false should clear ops");
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
                    qualname: "denoise.pass:cond".into(),
                    class_name: "Scope".into(),
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
                    in_flight_ns: 1_000_000,
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
                        name: "K_abcd_64x64x1".into(),
                        symbol: Some("gemm_t_n_bf16_64_64_32".into()),
                        gpu_ns: 1_000_000,
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
            ..Default::default()
        })
        .unwrap();

        let scope = resp
            .root
            .children
            .iter()
            .find(|c| c.qualname == "denoise.pass:cond")
            .expect("scope present");
        let op = scope.ops.first().expect("op present");
        assert_eq!(op.name, "K_abcd_64x64x1");
        assert_eq!(op.symbol.as_deref(), Some("gemm_t_n_bf16_64_64_32"));
        assert_eq!(op.kind.as_deref(), Some("Matmul"));
    }

    #[test]
    #[serial_test::serial]
    fn field_filter_keeps_matching_node_and_prunes_siblings() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();

        let make_pass = |seq: u64, ts: u64, cid: u64, idx: i64| Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source: Source::PythonSidecar,
            pid: None,
            seq,
            payload: Payload::ModuleEntered {
                module_call_id: cid,
                module_def_id: 0,
                qualname: "inner.pass".into(),
                class_name: "Scope".into(),
                parent_call_id: None,
                depth: 0,
                fields: {
                    let mut m = std::collections::BTreeMap::new();
                    m.insert("pass_idx".into(), FieldValue::Int(idx));
                    m
                },
            },
        };
        let ret_scope = |seq: u64, ts: u64, cid: u64| Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source: Source::PythonSidecar,
            pid: None,
            seq,
            payload: Payload::ModuleReturned {
                module_call_id: cid,
            },
        };

        let evs: Vec<Event> = vec![
            make_pass(1, 100, 1, 0),
            ret_scope(2, 200, 1),
            make_pass(3, 300, 2, 1),
            ret_scope(4, 400, 2),
            make_pass(5, 500, 3, 2),
            ret_scope(6, 600, 3),
        ];
        for e in &evs {
            w.write_event(e).unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();

        // Filter pass_idx=1 — only the middle sibling.
        let mut filter = BTreeMap::new();
        filter.insert("pass_idx".into(), fv_to_json(&FieldValue::Int(1)));
        let resp = run(Params {
            session: id.short(),
            field_filter: Some(filter),
            ..Default::default()
        })
        .unwrap();
        let passes: Vec<&ModuleBreakdown> = resp
            .root
            .children
            .iter()
            .filter(|c| c.qualname == "inner.pass")
            .collect();
        assert_eq!(passes.len(), 1, "only one sibling should pass the filter");
        assert_eq!(passes[0].fields.get("pass_idx"), Some(&FieldValue::Int(1)));
    }

    #[test]
    #[serial_test::serial]
    fn group_by_kind_collapses_leaf_matmuls() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        // Two ops under one scope, both resolving to Matmul via gemm_* symbol.
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
                    qualname: "scope.A".into(),
                    class_name: "Scope".into(),
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
                    cb_id: 10,
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
                    cb_id: 10,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 2_000_000,
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
                    cb_id: 10,
                    ops: vec![
                        smeltr_core::event::OpSample {
                            name: "K_gemm_a".into(),
                            symbol: Some("gemm_nn_f32_64_64_32".into()),
                            gpu_ns: 700_000,
                            count: 1,
                        },
                        smeltr_core::event::OpSample {
                            name: "K_gemm_b".into(),
                            symbol: Some("gemm_tt_bf16_64_64_32".into()),
                            gpu_ns: 300_000,
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
            group_by: Some("kind".into()),
            ..Default::default()
        })
        .unwrap();

        let scope = resp
            .root
            .children
            .iter()
            .find(|c| c.qualname == "scope.A")
            .expect("scope.A present");
        // After group_by=kind, the two gemm ops collapse into one "Matmul" op.
        assert_eq!(
            scope.ops.len(),
            1,
            "two gemm ops should collapse to one Matmul entry"
        );
        let op = &scope.ops[0];
        assert_eq!(op.name, "Matmul");
        assert_eq!(op.gpu_ns, 1_000_000, "gpu_ns should be summed");
        assert!(
            op.symbol.is_none(),
            "symbol should be None after kind grouping"
        );
    }

    #[test]
    #[serial_test::serial]
    fn unknown_group_by_is_bad_args() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let r = run(Params {
            session: "x".into(),
            group_by: Some("nope".into()),
            ..Default::default()
        });
        assert!(matches!(r, Err(ToolError::BadArgs(_))));
    }

    #[test]
    #[serial_test::serial]
    fn field_filter_no_match_returns_empty_children() {
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
                    module_def_id: 0,
                    qualname: "foo".into(),
                    class_name: "Scope".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields: {
                        let mut m = std::collections::BTreeMap::new();
                        m.insert("step".into(), FieldValue::Int(5));
                        m
                    },
                },
            },
            Event {
                ts_mono_ns: 2,
                ts_wall_ns: 2,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 2,
                payload: Payload::ModuleReturned { module_call_id: 1 },
            },
        ];
        for e in &evs {
            w.write_event(e).unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();

        let mut filter = BTreeMap::new();
        filter.insert("step".into(), fv_to_json(&FieldValue::Int(999))); // no node matches
        let resp = run(Params {
            session: id.short(),
            field_filter: Some(filter),
            ..Default::default()
        })
        .unwrap();
        let foos: Vec<&ModuleBreakdown> = resp
            .root
            .children
            .iter()
            .filter(|c| c.qualname == "foo")
            .collect();
        assert_eq!(foos.len(), 0);
    }
}

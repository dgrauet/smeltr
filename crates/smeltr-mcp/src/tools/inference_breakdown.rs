//! `get_inference_breakdown` MCP tool.

use crate::session_cache::events as read_events;
use crate::types::{bounded_count, resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_analyzer::{
    apply_op_group_by, compute_breakdown, prune_by_field_filter, BreakdownNotices, ModuleBreakdown,
    OpGroupBy,
};
use smeltr_core::event::FieldValue;
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    pub session: String,
    /// Tree depth kept below the root (default 6, like `smeltr breakdown`;
    /// 1..=64).
    pub max_depth: Option<u16>,
    /// Nodes kept in the whole tree, largest GPU time first, like the rows
    /// of `smeltr breakdown --top` (default 20; 1..=200). A kept node's
    /// ancestors are always kept.
    pub top_n: Option<u32>,
    pub min_gpu_ns: Option<u64>,
    #[serde(default = "default_include_ops")]
    pub include_ops: bool,
    /// Ops kept per node (default 5; 0..=50).
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
    /// Explains a mostly-`<unscoped>` tree and how to instrument around it.
    /// Set when most GPU time ran under mx.eval() calls made outside any
    /// module forward (fully-lazy pipeline, #163), or when the session has
    /// Metal CBs but the Python sidecar never attached (#178). The two are
    /// mutually exclusive (the latter implies no eval windows at all).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution_gap: Option<String>,
    /// Set when op-timing sampling auto-disabled during the run (#165):
    /// the per-op `gpu_ns` below are incomplete over those spans. The CLI
    /// has always printed this; the tool used to omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<String>,
    /// Tree nodes (each with its whole subtree) cut by `max_depth`, `top_n`
    /// or `min_gpu_ns`. Raise the bounds, or narrow with `field_filter`,
    /// to see them. Absent when nothing was cut.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub elided_nodes: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
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
    // Same defaults as `smeltr breakdown --depth 6 --top 20` (#271: they
    // used to be unbounded, and a real session's tree was 1.19 M chars).
    let max_depth = bounded_count("max_depth", params.max_depth, 6, 64)?;
    let top_n = bounded_count("top_n", params.top_n, 20, 200)? as usize;
    if params.top_ops_per_leaf > 50 {
        return Err(ToolError::BadArgs(format!(
            "top_ops_per_leaf must be at most 50, got {}",
            params.top_ops_per_leaf
        )));
    }
    let top_ops_per_leaf = params.top_ops_per_leaf as usize;

    let dir = resolve_session(&params.session)?;
    let events = read_events(&dir)?;
    let notices = BreakdownNotices::detect(&events);
    let attribution_gap = notices
        .attribution_gap
        .as_ref()
        .map(|g| g.advice().to_string());
    let degraded = notices.degraded_advice();
    let mut root = compute_breakdown(events.iter().cloned())
        .map_err(|e| ToolError::BadArgs(format!("breakdown: {e}")))?;

    if let Some(raw_filter) = params.field_filter.as_ref() {
        if !raw_filter.is_empty() {
            // Convert JSON values → FieldValue. A value that is not a scalar
            // is an error, as in the CLI: dropping it from the filter used to
            // return the whole tree unfiltered when nothing was left (#243).
            let mut filter: BTreeMap<String, FieldValue> = BTreeMap::new();
            for (k, v) in raw_filter {
                let fv = serde_json::from_value::<FieldValue>(v.clone()).map_err(|_| {
                    ToolError::BadArgs(format!(
                        "field_filter[{k:?}] must be a bool, number or string, got {v}"
                    ))
                })?;
                filter.insert(k.clone(), fv);
            }
            prune_by_field_filter(&mut root, &filter);
        }
    }

    let min_gpu_ns = params.min_gpu_ns.unwrap_or(0);
    let elided_nodes = prune(&mut root, max_depth, top_n, min_gpu_ns);

    apply_op_group_by(&mut root, group_by);

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

    Ok(Response {
        root,
        attribution_gap,
        degraded,
        elided_nodes,
    })
}

/// Every node of `n`'s subtree, `n` included.
fn subtree_size(n: &ModuleBreakdown) -> u64 {
    1 + n.children.iter().map(subtree_size).sum::<u64>()
}

/// Candidate nodes as (GPU time desc, depth, preorder id).
type Ranked = Vec<(std::cmp::Reverse<u64>, u16, usize)>;

/// Keeps the `top_n` nodes of the whole tree with the largest
/// `gpu_ns_subtree` (at most `max_depth` below the root, at or above
/// `min_gpu_ns`), like the rows of `smeltr breakdown --top`; children end up
/// sorted by GPU time. Returns how many nodes it cut.
///
/// Ranked by (GPU time desc, depth asc), a node always comes after its
/// ancestors — their subtree holds its time — so what is kept stays a tree.
/// `top_n` used to apply per node: its default of 20 still let a real
/// session's tree reach 392k characters at depth 6 (#271).
fn prune(root: &mut ModuleBreakdown, max_depth: u16, top_n: usize, min_gpu_ns: u64) -> u64 {
    fn sort(n: &mut ModuleBreakdown) {
        n.children
            .sort_by_key(|c| std::cmp::Reverse(c.gpu_ns_subtree));
        n.children.iter_mut().for_each(sort);
    }
    fn rank(
        n: &ModuleBreakdown,
        depth: u16,
        limits: (u16, u64),
        next: &mut usize,
        out: &mut Ranked,
    ) {
        for c in &n.children {
            let id = *next;
            *next += 1;
            if depth < limits.0 && c.gpu_ns_subtree >= limits.1 {
                out.push((std::cmp::Reverse(c.gpu_ns_subtree), depth + 1, id));
            }
            rank(c, depth + 1, limits, next, out);
        }
    }
    fn retain(n: &mut ModuleBreakdown, next: &mut usize, keep: &HashSet<usize>) {
        for mut c in std::mem::take(&mut n.children) {
            if keep.contains(next) {
                *next += 1;
                retain(&mut c, next, keep);
                n.children.push(c);
            } else {
                *next += subtree_size(&c) as usize;
            }
        }
    }
    sort(root);
    let mut ranked = Ranked::new();
    rank(root, 0, (max_depth, min_gpu_ns), &mut 0, &mut ranked);
    ranked.sort_unstable();
    let keep: HashSet<usize> = ranked.iter().take(top_n).map(|r| r.2).collect();
    let total = subtree_size(root) - 1;
    retain(root, &mut 0, &keep);
    total - keep.len() as u64
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

    /// A session whose module tree is a chain `c0 > c1 > … > c{depth-1}`
    /// next to `width` sibling leaves `w0…` at the top level.
    fn wide_and_deep_session(depth: u64, width: u64) -> SessionId {
        let id = SessionId::new();
        let mut w = SessionWriter::create(SessionMetadata::now_starting(id)).unwrap();
        let mut seq = 0u64;
        let mut emit = |payload: Payload| {
            seq += 1;
            w.write_event(&Event {
                ts_mono_ns: seq,
                ts_wall_ns: seq,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq,
                payload,
            })
            .unwrap();
        };
        let enter =
            |call: u64, name: String, parent: Option<u64>, depth: u16| Payload::ModuleEntered {
                module_call_id: call,
                module_def_id: call,
                qualname: name.clone(),
                class_name: name,
                parent_call_id: parent,
                depth,
                fields: Default::default(),
            };
        for d in 0..depth {
            let parent = d.checked_sub(1).map(|p| p + 1);
            emit(enter(d + 1, format!("c{d}"), parent, d as u16));
        }
        for d in (0..depth).rev() {
            emit(Payload::ModuleReturned {
                module_call_id: d + 1,
            });
        }
        for i in 0..width {
            emit(enter(1000 + i, format!("w{i}"), None, 0));
            emit(Payload::ModuleReturned {
                module_call_id: 1000 + i,
            });
        }
        w.finalize(Some(0), "x".into()).unwrap();
        id
    }

    fn node(name: &str, gpu: u64, children: Vec<ModuleBreakdown>) -> ModuleBreakdown {
        ModuleBreakdown {
            qualname: name.into(),
            class_name: name.into(),
            calls: 1,
            gpu_ns_self: 0,
            gpu_ns_subtree: gpu,
            eval_count: 0,
            cb_count: 0,
            children,
            ops: vec![],
            diagnostics: None,
            fields: Default::default(),
        }
    }

    fn names(n: &ModuleBreakdown) -> Vec<String> {
        let mut out = vec![];
        for c in &n.children {
            out.push(c.qualname.clone());
            out.extend(names(c));
        }
        out
    }

    /// #271: `top_n` counts nodes over the whole tree, the largest GPU
    /// time first, like the rows of `smeltr breakdown --top`: per node, the
    /// default (20) still let a real session's tree reach 392k characters
    /// at depth 6. A kept node's ancestors are always kept.
    #[test]
    fn top_n_keeps_the_heaviest_nodes_of_the_whole_tree() {
        let mut root = node(
            "<root>",
            200,
            vec![
                node("b", 50, vec![node("b1", 49, vec![])]),
                node(
                    "a",
                    150,
                    vec![
                        node("a2", 40, vec![]),
                        node("a1", 100, vec![node("a11", 90, vec![])]),
                    ],
                ),
                node("c", 10, vec![]),
            ],
        );
        let elided = prune(&mut root, 6, 4, 0);
        assert_eq!(names(&root), ["a", "a1", "a11", "b"]);
        assert_eq!(elided, 3, "a2, b1 and c");

        // Depth and min_gpu_ns cut before the ranking.
        let mut shallow = root.clone();
        assert_eq!(prune(&mut shallow, 1, 200, 0), 2);
        assert_eq!(names(&shallow), ["a", "b"]);
        let mut heavy = root.clone();
        prune(&mut heavy, 6, 200, 95);
        assert_eq!(names(&heavy), ["a", "a1"]);
    }

    fn tree_depth(n: &ModuleBreakdown) -> usize {
        n.children
            .iter()
            .map(|c| 1 + tree_depth(c))
            .max()
            .unwrap_or(0)
    }

    /// #271: `max_depth` and `top_n` defaulted to unbounded, and the tree of
    /// a real session came back as 1.19 M characters. The defaults now
    /// match `smeltr breakdown` (`--depth 6 --top 20`), and what they cut
    /// is counted rather than dropped silently.
    #[test]
    #[serial_test::serial]
    fn defaults_bound_the_tree_like_the_cli() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        // Separate sessions: every node has 0 GPU time here, so which of
        // equal siblings `top_n` keeps is unspecified.
        let deep = wide_and_deep_session(9, 0);
        let resp = run(Params {
            session: deep.short(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(tree_depth(&resp.root), 6);
        assert_eq!(resp.elided_nodes, 3, "the chain loses c6..c8");

        let wide = wide_and_deep_session(0, 30);
        let resp = run(Params {
            session: wide.short(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resp.root.children.len(), 20);
        assert_eq!(resp.elided_nodes, 10);

        let id = wide_and_deep_session(9, 30);
        let all = run(Params {
            session: id.short(),
            max_depth: Some(64),
            top_n: Some(200),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(all.root.children.len(), 31);
        assert_eq!(tree_depth(&all.root), 9);
        assert_eq!(all.elided_nodes, 0);
        let json = serde_json::to_value(&all).unwrap();
        assert!(json.get("elided_nodes").is_none(), "omitted when 0");
    }

    /// #271: caller-supplied bounds are capped; `top_n: 0` is refused.
    #[test]
    #[serial_test::serial]
    fn out_of_range_bounds_are_bad_args() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let cases = [
            Params {
                top_n: Some(0),
                ..Default::default()
            },
            Params {
                top_n: Some(201),
                ..Default::default()
            },
            Params {
                max_depth: Some(65),
                ..Default::default()
            },
            Params {
                top_ops_per_leaf: 51,
                ..Default::default()
            },
        ];
        for mut p in cases {
            p.session = "deadbeef".into();
            let desc = format!("{:?}/{:?}/{}", p.top_n, p.max_depth, p.top_ops_per_leaf);
            let r = run(p);
            assert!(
                matches!(&r, Err(ToolError::BadArgs(_))),
                "{desc}: {:?}",
                r.map(|r| r.elided_nodes)
            );
        }
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
        assert!(
            resp.attribution_gap.is_none(),
            "module-stacked eval must not report a gap"
        );
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
        // Identity is the symbol once resolved (#265).
        assert_eq!(op.name, "gemm_t_n_bf16_64_64_32");
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

    /// #165 parity: the CLI has always warned that per-op numbers are
    /// partial when op-timing sampling auto-disabled. The tool used to stay
    /// silent, leaving an agent no way to know the numbers were incomplete.
    #[test]
    #[serial_test::serial]
    fn sampling_disabled_session_surfaces_degraded_notice() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let meta = SessionMetadata::now_starting(SessionId::new());
        let mut w = SessionWriter::create(meta).unwrap();
        for (seq, reason) in [
            (
                1u64,
                "stage sampling disabled after sustained alloc failures",
            ),
            (
                2,
                "dispatch sampling disabled after sustained alloc failures",
            ),
        ] {
            w.write_event(&Event {
                ts_mono_ns: seq,
                ts_wall_ns: seq,
                session_id: Uuid::nil(),
                source: Source::MetalHook,
                pid: None,
                seq,
                payload: Payload::MetalHookSkipped {
                    reason: reason.into(),
                },
            })
            .unwrap();
        }
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "ok".into()).unwrap();

        let resp = run(Params {
            session: dir.file_name().unwrap().to_string_lossy().into_owned(),
            ..Default::default()
        })
        .unwrap();
        let notice = resp.degraded.expect("degraded notice");
        assert!(notice.contains("2 time(s)"), "{notice}");
        assert!(notice.contains("partial"), "{notice}");
    }

    /// #163: a fully-lazy session (single pipeline-level eval with empty
    /// module_stack containing all CBs) must surface `attribution_gap`.
    #[test]
    #[serial_test::serial]
    fn lazy_session_surfaces_attribution_gap() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        let mk = |ts: u64, seq: u64, source: Source, payload: Payload| Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source,
            pid: None,
            seq,
            payload,
        };
        let evs: Vec<Event> = vec![
            mk(
                100,
                1,
                Source::PythonSidecar,
                Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 1,
                    qualname: "A".into(),
                    class_name: "A".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields: Default::default(),
                },
            ),
            mk(
                200,
                2,
                Source::PythonSidecar,
                Payload::ModuleReturned { module_call_id: 1 },
            ),
            // Pipeline-level eval, outside any module forward.
            mk(
                1_000_000_000,
                3,
                Source::PythonSidecar,
                Payload::MlxEvalEntered {
                    call_id: 7,
                    array_count: 1,
                    stream: "gpu".into(),
                    module_stack: vec![],
                    stack_frames: vec![],
                },
            ),
            mk(
                2_000_000_000,
                4,
                Source::MetalHook,
                Payload::MetalCbCommitted {
                    cb_id: 9,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
            ),
            mk(
                3_000_000_000,
                5,
                Source::MetalHook,
                Payload::MetalCbCompleted {
                    cb_id: 9,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 90_000,
                },
            ),
            mk(
                4_000_000_000,
                6,
                Source::PythonSidecar,
                Payload::MlxEvalReturned {
                    call_id: 7,
                    duration_ns: 30,
                    was_async: false,
                },
            ),
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
        let gap = resp.attribution_gap.expect("gap should be surfaced");
        assert!(gap.contains("smeltr.scope"), "{gap}");
    }

    /// #178: a session with Metal CBs but zero sidecar events must surface
    /// the sidecar-absent advice in `attribution_gap`.
    #[test]
    #[serial_test::serial]
    fn metal_only_session_surfaces_sidecar_absent() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        let evs: Vec<Event> = vec![
            Event {
                ts_mono_ns: 20,
                ts_wall_ns: 20,
                session_id: Uuid::nil(),
                source: Source::MetalHook,
                pid: None,
                seq: 1,
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
                seq: 2,
                payload: Payload::MetalCbCompleted {
                    cb_id: 9,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 90_000,
                },
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
        let gap = resp
            .attribution_gap
            .expect("sidecar-absent advice expected");
        assert!(gap.contains("sidecar never attached"), "{gap}");
        assert!(gap.contains("pip install"), "{gap}");
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

    /// #243: a filter value that is not a scalar used to be dropped from the
    /// filter; with nothing left, the whole tree came back unfiltered — the
    /// opposite of a no-match — while the CLI rejects it.
    #[test]
    #[serial_test::serial]
    fn unconvertible_field_filter_is_rejected() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let session = crate::test_util::sampling_disabled_session();
        let mut filter = BTreeMap::new();
        filter.insert("step".to_string(), serde_json::Value::Null);
        let r = run(Params {
            session,
            field_filter: Some(filter),
            ..Default::default()
        });
        match r {
            Err(ToolError::BadArgs(msg)) => assert!(msg.contains("step"), "{msg}"),
            other => panic!("expected BadArgs, got {other:?}"),
        }
    }
}

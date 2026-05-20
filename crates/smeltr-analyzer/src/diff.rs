//! Session-vs-session diff at scope and op-kind granularity.

use crate::breakdown::{compute, ModuleBreakdown, OpAttribution};
use crate::op_kinds::resolve_kind;
use serde::{Deserialize, Serialize};
use smeltr_core::event::Event;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionDiff {
    pub scope_deltas: Vec<ScopeDelta>,
    pub op_deltas: Vec<OpDelta>,
    pub scopes_only_in_a: Vec<ScopeAggregate>,
    pub scopes_only_in_b: Vec<ScopeAggregate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScopeDelta {
    pub qualname: String,
    pub a_gpu_ns: u64,
    pub b_gpu_ns: u64,
    pub delta_ns: i64,
    /// `None` when `a_gpu_ns == 0` (cannot divide by zero).
    pub delta_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpDelta {
    pub kind: String,
    pub a_gpu_ns: u64,
    pub b_gpu_ns: u64,
    pub delta_ns: i64,
    pub delta_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryDelta {
    pub qualname: String,
    pub a_peak_bytes: u64,
    pub b_peak_bytes: u64,
    pub delta_bytes: i64,
    pub delta_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScopeAggregate {
    pub qualname: String,
    pub gpu_ns: u64,
}

/// Diff two sessions and produce scope + op deltas plus
/// only-in-A / only-in-B scope lists.
///
/// Empty sessions on either side yield an empty `SessionDiff` (no panic).
/// Sort order for all delta lists: by `|delta_ns|` descending. The
/// only-in-* lists are sorted by `gpu_ns` descending.
pub fn diff_sessions(a_events: &[Event], b_events: &[Event]) -> SessionDiff {
    let a_scopes = scope_map(a_events);
    let b_scopes = scope_map(b_events);
    let a_ops = op_map(a_events);
    let b_ops = op_map(b_events);

    let mut scope_deltas: Vec<ScopeDelta> = a_scopes
        .iter()
        .filter_map(|(qualname, a_gpu)| {
            b_scopes.get(qualname).map(|b_gpu| ScopeDelta {
                qualname: qualname.clone(),
                a_gpu_ns: *a_gpu,
                b_gpu_ns: *b_gpu,
                delta_ns: *b_gpu as i64 - *a_gpu as i64,
                delta_pct: pct(*a_gpu, *b_gpu),
            })
        })
        .collect();
    scope_deltas.sort_by_key(|d| std::cmp::Reverse(d.delta_ns.unsigned_abs()));

    let mut op_deltas: Vec<OpDelta> = {
        let mut keys: std::collections::BTreeSet<String> = a_ops.keys().cloned().collect();
        keys.extend(b_ops.keys().cloned());
        keys.into_iter()
            .map(|kind| {
                let a_gpu = *a_ops.get(&kind).unwrap_or(&0);
                let b_gpu = *b_ops.get(&kind).unwrap_or(&0);
                OpDelta {
                    kind,
                    a_gpu_ns: a_gpu,
                    b_gpu_ns: b_gpu,
                    delta_ns: b_gpu as i64 - a_gpu as i64,
                    delta_pct: pct(a_gpu, b_gpu),
                }
            })
            .collect()
    };
    op_deltas.sort_by_key(|d| std::cmp::Reverse(d.delta_ns.unsigned_abs()));

    let mut scopes_only_in_a: Vec<ScopeAggregate> = a_scopes
        .iter()
        .filter(|(q, _)| !b_scopes.contains_key(*q))
        .map(|(q, g)| ScopeAggregate {
            qualname: q.clone(),
            gpu_ns: *g,
        })
        .collect();
    scopes_only_in_a.sort_by_key(|s| std::cmp::Reverse(s.gpu_ns));

    let mut scopes_only_in_b: Vec<ScopeAggregate> = b_scopes
        .iter()
        .filter(|(q, _)| !a_scopes.contains_key(*q))
        .map(|(q, g)| ScopeAggregate {
            qualname: q.clone(),
            gpu_ns: *g,
        })
        .collect();
    scopes_only_in_b.sort_by_key(|s| std::cmp::Reverse(s.gpu_ns));

    SessionDiff {
        scope_deltas,
        op_deltas,
        scopes_only_in_a,
        scopes_only_in_b,
    }
}

/// Diff per-scope peak GPU memory between two sessions. Returns entries
/// present in BOTH sessions, sorted by `|delta_bytes|` desc.
pub fn diff_memory(a_events: &[Event], b_events: &[Event]) -> Vec<MemoryDelta> {
    use crate::memory::compute_memory_breakdown;
    let a_map: std::collections::HashMap<String, u64> = compute_memory_breakdown(a_events)
        .into_iter()
        .map(|s| (s.qualname, s.peak_bytes))
        .collect();
    let b_map: std::collections::HashMap<String, u64> = compute_memory_breakdown(b_events)
        .into_iter()
        .map(|s| (s.qualname, s.peak_bytes))
        .collect();
    let mut out: Vec<MemoryDelta> = a_map
        .iter()
        .filter_map(|(qualname, a_peak)| {
            b_map.get(qualname).map(|b_peak| MemoryDelta {
                qualname: qualname.clone(),
                a_peak_bytes: *a_peak,
                b_peak_bytes: *b_peak,
                delta_bytes: *b_peak as i64 - *a_peak as i64,
                delta_pct: pct(*a_peak, *b_peak),
            })
        })
        .collect();
    out.sort_by_key(|d| std::cmp::Reverse(d.delta_bytes.unsigned_abs()));
    out
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OriginDelta {
    pub kind: String,
    pub file_line: String,
    pub a_gpu_ns: u64,
    pub b_gpu_ns: u64,
    pub delta_ns: i64,
    pub delta_pct: Option<f64>,
}

/// Diff per-(kind, file:line) GPU time between two sessions. Returns
/// entries present in BOTH sessions, sorted by `|delta_ns|` desc.
pub fn diff_origins(a_events: &[Event], b_events: &[Event]) -> Vec<OriginDelta> {
    use crate::dispatch_origins::compute_dispatch_origins;
    let a_map: std::collections::HashMap<(String, String), u64> =
        compute_dispatch_origins(a_events)
            .into_iter()
            .map(|o| ((o.kind, o.file_line), o.gpu_ns))
            .collect();
    let b_map: std::collections::HashMap<(String, String), u64> =
        compute_dispatch_origins(b_events)
            .into_iter()
            .map(|o| ((o.kind, o.file_line), o.gpu_ns))
            .collect();
    let mut out: Vec<OriginDelta> = a_map
        .iter()
        .filter_map(|(key, a_gpu)| {
            b_map.get(key).map(|b_gpu| OriginDelta {
                kind: key.0.clone(),
                file_line: key.1.clone(),
                a_gpu_ns: *a_gpu,
                b_gpu_ns: *b_gpu,
                delta_ns: *b_gpu as i64 - *a_gpu as i64,
                delta_pct: pct(*a_gpu, *b_gpu),
            })
        })
        .collect();
    out.sort_by_key(|d| std::cmp::Reverse(d.delta_ns.unsigned_abs()));
    out
}

fn pct(a: u64, b: u64) -> Option<f64> {
    if a == 0 {
        None
    } else {
        Some((b as f64 - a as f64) / a as f64 * 100.0)
    }
}

fn scope_map(events: &[Event]) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    let root = match compute(events.iter().cloned()) {
        Ok(r) => r,
        Err(_) => return out,
    };
    walk_scopes(&root, &mut out);
    out.remove("<root>");
    out
}

fn walk_scopes(node: &ModuleBreakdown, out: &mut HashMap<String, u64>) {
    *out.entry(node.qualname.clone()).or_insert(0) += node.gpu_ns_subtree;
    for child in &node.children {
        walk_scopes(child, out);
    }
}

fn op_map(events: &[Event]) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    let root = match compute(events.iter().cloned()) {
        Ok(r) => r,
        Err(_) => return out,
    };
    walk_ops(&root, &mut out);
    out
}

fn walk_ops(node: &ModuleBreakdown, out: &mut HashMap<String, u64>) {
    for op in &node.ops {
        let kind = op_kind_key(op);
        *out.entry(kind).or_insert(0) += op.gpu_ns;
    }
    for child in &node.children {
        walk_ops(child, out);
    }
}

fn op_kind_key(op: &OpAttribution) -> String {
    if let Some(k) = &op.kind {
        return k.clone();
    }
    if let Some(s) = &op.symbol {
        if let Some(resolved) = resolve_kind(s) {
            return resolved.to_string();
        }
        return s.clone();
    }
    op.name.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{OpSample, Payload, Source};
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

    fn scoped_session(qualname: &str, in_flight_ns: u64, op: OpSample) -> Vec<Event> {
        vec![
            ev(
                1,
                1,
                Source::PythonSidecar,
                Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 1,
                    qualname: qualname.into(),
                    class_name: "Scope".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields: Default::default(),
                },
            ),
            ev(
                2,
                10,
                Source::PythonSidecar,
                Payload::MlxEvalEntered {
                    call_id: 1,
                    array_count: 1,
                    stream: "gpu".into(),
                    module_stack: vec![1],
                    stack_frames: vec![],
                },
            ),
            ev(
                3,
                20,
                Source::MetalHook,
                Payload::MetalCbCommitted {
                    cb_id: 9,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
            ),
            ev(
                4,
                30,
                Source::MetalHook,
                Payload::MetalCbCompleted {
                    cb_id: 9,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns,
                },
            ),
            ev(
                5,
                31,
                Source::MetalHook,
                Payload::MetalCbOps {
                    cb_id: 9,
                    ops: vec![op],
                },
            ),
            ev(
                6,
                40,
                Source::PythonSidecar,
                Payload::MlxEvalReturned {
                    call_id: 1,
                    duration_ns: 30,
                    was_async: false,
                },
            ),
            ev(
                7,
                50,
                Source::PythonSidecar,
                Payload::ModuleReturned { module_call_id: 1 },
            ),
        ]
    }

    fn op_matmul(gpu_ns: u64) -> OpSample {
        OpSample {
            name: "K_abcd_64x64x1".into(),
            symbol: Some("gemm_t_n_bf16_64".into()),
            gpu_ns,
            count: 1,
        }
    }

    #[test]
    fn diff_empty_sessions_yields_empty_diff() {
        let d = diff_sessions(&[], &[]);
        assert!(d.scope_deltas.is_empty());
        assert!(d.op_deltas.is_empty());
        assert!(d.scopes_only_in_a.is_empty());
        assert!(d.scopes_only_in_b.is_empty());
    }

    #[test]
    fn diff_identical_sessions_yields_zero_deltas() {
        let a = scoped_session("denoise.pass:cond", 1_000, op_matmul(800));
        let b = scoped_session("denoise.pass:cond", 1_000, op_matmul(800));
        let d = diff_sessions(&a, &b);
        let scope = d
            .scope_deltas
            .iter()
            .find(|s| s.qualname == "denoise.pass:cond")
            .expect("scope present");
        assert_eq!(scope.delta_ns, 0);
        assert_eq!(scope.delta_pct, Some(0.0));
        assert!(d.scopes_only_in_a.is_empty());
        assert!(d.scopes_only_in_b.is_empty());
        let mm = d
            .op_deltas
            .iter()
            .find(|o| o.kind == "Matmul")
            .expect("Matmul present");
        assert_eq!(mm.delta_ns, 0);
    }

    #[test]
    fn diff_scope_only_in_a() {
        let a = scoped_session("denoise.pass:uncond", 5_000, op_matmul(4_000));
        let d = diff_sessions(&a, &[]);
        let entries: Vec<&str> = d
            .scopes_only_in_a
            .iter()
            .map(|s| s.qualname.as_str())
            .collect();
        assert!(entries.contains(&"denoise.pass:uncond"));
        let onpoint = d
            .scopes_only_in_a
            .iter()
            .find(|s| s.qualname == "denoise.pass:uncond")
            .unwrap();
        assert!(onpoint.gpu_ns > 0);
        assert!(!d
            .scope_deltas
            .iter()
            .any(|s| s.qualname == "denoise.pass:uncond"));
    }

    #[test]
    fn diff_scope_only_in_b() {
        let b = scoped_session("denoise.pass:dpo_warmup", 5_000, op_matmul(4_000));
        let d = diff_sessions(&[], &b);
        assert!(d
            .scopes_only_in_b
            .iter()
            .any(|s| s.qualname == "denoise.pass:dpo_warmup"));
    }

    #[test]
    fn diff_scope_delta_sorted_by_abs() {
        let mut a = scoped_session("tiny", 100, op_matmul(50));
        for ev in &mut a {
            ev.seq += 100;
        }
        a.extend({
            let mut second = scoped_session("huge", 10_000_000, op_matmul(8_000_000));
            for ev in &mut second {
                ev.seq += 200;
                ev.ts_mono_ns += 1_000;
                ev.ts_wall_ns += 1_000;
                if let Payload::ModuleEntered { module_call_id, .. }
                | Payload::ModuleReturned { module_call_id, .. } = &mut ev.payload
                {
                    *module_call_id = 2;
                }
                if let Payload::MlxEvalEntered { module_stack, .. } = &mut ev.payload {
                    *module_stack = vec![2];
                }
                if let Payload::MetalCbCommitted { cb_id, .. }
                | Payload::MetalCbCompleted { cb_id, .. }
                | Payload::MetalCbOps { cb_id, .. } = &mut ev.payload
                {
                    *cb_id = 10;
                }
            }
            second
        });
        let mut b = scoped_session("tiny", 105, op_matmul(55));
        for ev in &mut b {
            ev.seq += 300;
        }
        b.extend({
            let mut second = scoped_session("huge", 5_000_000, op_matmul(3_000_000));
            for ev in &mut second {
                ev.seq += 400;
                ev.ts_mono_ns += 1_000;
                ev.ts_wall_ns += 1_000;
                if let Payload::ModuleEntered { module_call_id, .. }
                | Payload::ModuleReturned { module_call_id, .. } = &mut ev.payload
                {
                    *module_call_id = 2;
                }
                if let Payload::MlxEvalEntered { module_stack, .. } = &mut ev.payload {
                    *module_stack = vec![2];
                }
                if let Payload::MetalCbCommitted { cb_id, .. }
                | Payload::MetalCbCompleted { cb_id, .. }
                | Payload::MetalCbOps { cb_id, .. } = &mut ev.payload
                {
                    *cb_id = 10;
                }
            }
            second
        });

        let d = diff_sessions(&a, &b);
        let names: Vec<&str> = d.scope_deltas.iter().map(|s| s.qualname.as_str()).collect();
        let huge_idx = names.iter().position(|n| *n == "huge").unwrap();
        let tiny_idx = names.iter().position(|n| *n == "tiny").unwrap();
        assert!(
            huge_idx < tiny_idx,
            "huge (large |delta|) must come before tiny: {names:?}"
        );
    }

    #[test]
    fn diff_delta_pct_none_when_a_zero() {
        let a = scoped_session("foo", 0, op_matmul(0));
        let b = scoped_session("foo", 500, op_matmul(400));
        let d = diff_sessions(&a, &b);
        let scope = d
            .scope_deltas
            .iter()
            .find(|s| s.qualname == "foo")
            .expect("foo present");
        assert_eq!(scope.delta_pct, None);
        assert!(scope.delta_ns > 0);
    }

    #[test]
    fn diff_op_aggregation_by_kind() {
        let mut events = scoped_session("scope_one", 100, op_matmul(60));
        for ev in &mut events {
            ev.seq += 100;
        }
        let mut more = scoped_session("scope_two", 100, op_matmul(40));
        for ev in &mut more {
            ev.seq += 200;
            ev.ts_mono_ns += 1_000;
            ev.ts_wall_ns += 1_000;
            if let Payload::ModuleEntered { module_call_id, .. }
            | Payload::ModuleReturned { module_call_id, .. } = &mut ev.payload
            {
                *module_call_id = 2;
            }
            if let Payload::MlxEvalEntered { module_stack, .. } = &mut ev.payload {
                *module_stack = vec![2];
            }
            if let Payload::MetalCbCommitted { cb_id, .. }
            | Payload::MetalCbCompleted { cb_id, .. }
            | Payload::MetalCbOps { cb_id, .. } = &mut ev.payload
            {
                *cb_id = 10;
            }
        }
        events.extend(more);
        let d = diff_sessions(&events, &events);
        let matmul_entries: Vec<&OpDelta> =
            d.op_deltas.iter().filter(|o| o.kind == "Matmul").collect();
        assert_eq!(
            matmul_entries.len(),
            1,
            "expected ONE Matmul row aggregating across scopes"
        );
        assert_eq!(matmul_entries[0].a_gpu_ns, 100);
        assert_eq!(matmul_entries[0].b_gpu_ns, 100);
    }

    #[test]
    fn diff_op_fallback_to_symbol_then_name() {
        let no_sym = OpSample {
            name: "K_unknown_kernel".into(),
            symbol: None,
            gpu_ns: 100,
            count: 1,
        };
        let a = scoped_session("scope", 100, no_sym);
        let d = diff_sessions(&a, &a);
        assert!(d.op_deltas.iter().any(|o| o.kind == "K_unknown_kernel"));
    }

    #[test]
    fn diff_memory_basic() {
        let mk = |peak: u64| -> Vec<Event> {
            vec![
                ev(
                    1,
                    1,
                    Source::PythonSidecar,
                    Payload::ModuleEntered {
                        module_call_id: 1,
                        module_def_id: 0,
                        qualname: "scope".into(),
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
                        allocated_bytes: peak,
                        recommended_max_bytes: 16_000_000_000,
                        at_event: "cb_committed".into(),
                    },
                ),
                ev(
                    3,
                    3,
                    Source::PythonSidecar,
                    Payload::ModuleReturned { module_call_id: 1 },
                ),
            ]
        };
        let a = mk(2_000_000_000);
        let b = mk(1_000_000_000);
        let d = diff_memory(&a, &b);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].qualname, "scope");
        assert_eq!(d[0].delta_bytes, -1_000_000_000);
        assert_eq!(d[0].delta_pct, Some(-50.0));
    }

    #[test]
    fn diff_origins_basic() {
        use smeltr_core::event::StackFrame;
        let mk = |gpu: u64| -> Vec<Event> {
            vec![
                ev(
                    1,
                    10,
                    Source::PythonSidecar,
                    Payload::MlxEvalEntered {
                        call_id: 1,
                        array_count: 1,
                        stream: "gpu".into(),
                        module_stack: vec![],
                        stack_frames: vec![StackFrame {
                            filename: "/work/attention.py".into(),
                            lineno: 127,
                            funcname: "f".into(),
                        }],
                    },
                ),
                ev(
                    2,
                    15,
                    Source::MetalHook,
                    Payload::MetalCbOps {
                        cb_id: 9,
                        ops: vec![OpSample {
                            name: "K_x".into(),
                            symbol: Some("gemm_bf16".into()),
                            gpu_ns: gpu,
                            count: 1,
                        }],
                    },
                ),
                ev(
                    3,
                    20,
                    Source::PythonSidecar,
                    Payload::MlxEvalReturned {
                        call_id: 1,
                        duration_ns: 10,
                        was_async: false,
                    },
                ),
            ]
        };
        let a = mk(2_000_000_000);
        let b = mk(1_000_000_000);
        let d = diff_origins(&a, &b);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].kind, "Matmul");
        assert_eq!(d[0].file_line, "attention.py:127");
        assert_eq!(d[0].delta_ns, -1_000_000_000);
        assert_eq!(d[0].delta_pct, Some(-50.0));
    }
}

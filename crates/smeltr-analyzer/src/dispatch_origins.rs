//! Attribute kernel dispatches to Python source file:line.
//!
//! Joins each `MlxEvalEntered.stack_frames` (top non-smeltr frame) with
//! the `MetalCbOps` events that fall within the eval's window, aggregating
//! per `(kind, file_line)` → `(sum_gpu_ns, count)`.

use crate::op_kinds::resolve_kind;
use serde::{Deserialize, Serialize};
use smeltr_core::event::{Event, OpSample, Payload};
use std::collections::HashMap;
use std::path::Path;

const ASYNC_GRACE_NS: u64 = 500_000_000; // 500 ms — mirrors breakdown.rs

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DispatchOrigin {
    pub kind: String,
    pub file_line: String,
    pub gpu_ns: u64,
    pub dispatch_count: u64,
}

#[derive(Clone)]
struct EvalWindow {
    t_in: u64,
    t_out: u64,
    file_line: String,
}

/// Compute per-(kind, file:line) dispatch attribution.
///
/// Eval-window matching: for each `MetalCbOps` at ts T, find the
/// eval window with `t_in ≤ T ≤ t_out` (scanning latest-first). The top
/// non-smeltr frame on that eval is the attribution.
///
/// Empty if no events carry stack_frames (capture disabled).
pub fn compute_dispatch_origins(events: &[Event]) -> Vec<DispatchOrigin> {
    let mut evals: Vec<EvalWindow> = Vec::new();
    let mut open: HashMap<u64, (u64, String)> = HashMap::new();
    // #140 scope fallback — mirrors breakdown.rs step 4.5: lazy workloads
    // barely call mx.eval, so CBs with no eval window are attributed to the
    // innermost scope/module window open at that time, as `scope:<qualname>`.
    struct ScopeWindow {
        t_in: u64,
        t_out: u64,
        qualname: String,
    }
    let mut scope_windows: Vec<ScopeWindow> = Vec::new();
    let mut open_scope_idx: HashMap<u64, usize> = HashMap::new();
    let last_event_ts = events.last().map(|e| e.ts_mono_ns).unwrap_or(0);

    for ev in events {
        match &ev.payload {
            Payload::ModuleEntered {
                module_call_id,
                qualname,
                ..
            } => {
                open_scope_idx.insert(*module_call_id, scope_windows.len());
                scope_windows.push(ScopeWindow {
                    t_in: ev.ts_mono_ns,
                    // Closed on ModuleReturned; a never-returned call stays
                    // open until the end of the session.
                    t_out: last_event_ts,
                    qualname: qualname.clone(),
                });
            }
            Payload::ModuleReturned { module_call_id } => {
                if let Some(i) = open_scope_idx.remove(module_call_id) {
                    scope_windows[i].t_out = ev.ts_mono_ns;
                }
            }
            Payload::MlxEvalEntered {
                call_id,
                stack_frames,
                ..
            } => {
                if let Some(top) = stack_frames.first() {
                    let file_line = format!("{}:{}", basename(&top.filename), top.lineno);
                    open.insert(*call_id, (ev.ts_mono_ns, file_line));
                }
            }
            Payload::MlxEvalReturned {
                call_id, was_async, ..
            } => {
                if let Some((t_in, file_line)) = open.remove(call_id) {
                    let t_out = if *was_async {
                        ev.ts_mono_ns.saturating_add(ASYNC_GRACE_NS)
                    } else {
                        ev.ts_mono_ns
                    };
                    evals.push(EvalWindow {
                        t_in,
                        t_out,
                        file_line,
                    });
                }
            }
            _ => {}
        }
    }

    // Sweep state for the scope fallback: events (hence CbOps) and
    // scope_windows are both chronological; the stack top is the innermost
    // open window (inner calls return before outer ones, so expired windows
    // pop from the top).
    let mut next_scope = 0usize;
    let mut scope_stack: Vec<&ScopeWindow> = Vec::new();

    let mut agg: HashMap<(String, String), (u64, u64)> = HashMap::new();
    for ev in events {
        if let Payload::MetalCbOps { ops, .. } = &ev.payload {
            let ts = ev.ts_mono_ns;
            let file_line = match find_window(&evals, ts) {
                Some(window) => window.file_line.clone(),
                None => {
                    while next_scope < scope_windows.len() && scope_windows[next_scope].t_in <= ts {
                        scope_stack.push(&scope_windows[next_scope]);
                        next_scope += 1;
                    }
                    while let Some(top) = scope_stack.last() {
                        if top.t_out.saturating_add(ASYNC_GRACE_NS) < ts {
                            scope_stack.pop();
                        } else {
                            break;
                        }
                    }
                    match scope_stack.last() {
                        Some(win) => format!("scope:{}", win.qualname),
                        None => continue,
                    }
                }
            };
            for op in ops {
                let kind = op_kind(op);
                let entry = agg.entry((kind, file_line.clone())).or_insert((0, 0));
                entry.0 += op.gpu_ns;
                entry.1 += op.count as u64;
            }
        }
    }

    let mut out: Vec<DispatchOrigin> = agg
        .into_iter()
        .map(|((kind, file_line), (gpu_ns, count))| DispatchOrigin {
            kind,
            file_line,
            gpu_ns,
            dispatch_count: count,
        })
        .collect();
    out.sort_by_key(|o| std::cmp::Reverse(o.gpu_ns));
    out
}

fn find_window(evals: &[EvalWindow], ts: u64) -> Option<&EvalWindow> {
    evals.iter().rev().find(|w| w.t_in <= ts && ts <= w.t_out)
}

fn basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

fn op_kind(op: &OpSample) -> String {
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
    use smeltr_core::event::{Source, StackFrame};
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

    fn enter(seq: u64, ts: u64, call_id: u64, filename: &str, lineno: u32) -> Event {
        ev(
            seq,
            ts,
            Source::PythonSidecar,
            Payload::MlxEvalEntered {
                call_id,
                array_count: 1,
                stream: "gpu".into(),
                module_stack: vec![],
                stack_frames: vec![StackFrame {
                    filename: filename.into(),
                    lineno,
                    funcname: "fn".into(),
                }],
            },
        )
    }

    fn ret(seq: u64, ts: u64, call_id: u64) -> Event {
        ev(
            seq,
            ts,
            Source::PythonSidecar,
            Payload::MlxEvalReturned {
                call_id,
                duration_ns: 0,
                was_async: false,
            },
        )
    }

    fn ret_async(seq: u64, ts: u64, call_id: u64) -> Event {
        ev(
            seq,
            ts,
            Source::PythonSidecar,
            Payload::MlxEvalReturned {
                call_id,
                duration_ns: 0,
                was_async: true,
            },
        )
    }

    fn ops(seq: u64, ts: u64, cb_id: u64, symbol: &str, gpu_ns: u64) -> Event {
        ev(
            seq,
            ts,
            Source::MetalHook,
            Payload::MetalCbOps {
                cb_id,
                ops: vec![OpSample {
                    name: "K_xxx".into(),
                    symbol: Some(symbol.into()),
                    gpu_ns,
                    count: 1,
                }],
            },
        )
    }

    #[test]
    fn dispatch_origins_empty_session_yields_empty() {
        assert!(compute_dispatch_origins(&[]).is_empty());
    }

    #[test]
    fn dispatch_origins_aggregates_by_kind_and_file_line() {
        let evs = vec![
            enter(1, 10, 1, "/work/attention.py", 127),
            ops(2, 15, 9, "gemm_bf16", 100),
            ret(3, 20, 1),
            enter(4, 30, 2, "/work/attention.py", 127),
            ops(5, 35, 10, "gemm_bf16", 200),
            ret(6, 40, 2),
        ];
        let out = compute_dispatch_origins(&evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, "Matmul");
        assert_eq!(out[0].file_line, "attention.py:127");
        assert_eq!(out[0].gpu_ns, 300);
        assert_eq!(out[0].dispatch_count, 2);
    }

    #[test]
    fn dispatch_origins_eval_without_frames_is_skipped() {
        let evs = vec![
            ev(
                1,
                10,
                Source::PythonSidecar,
                Payload::MlxEvalEntered {
                    call_id: 1,
                    array_count: 1,
                    stream: "gpu".into(),
                    module_stack: vec![],
                    stack_frames: vec![],
                },
            ),
            ops(2, 15, 9, "gemm_bf16", 100),
            ret(3, 20, 1),
        ];
        let out = compute_dispatch_origins(&evs);
        assert!(out.is_empty());
    }

    #[test]
    fn dispatch_origins_sorted_by_gpu_ns_desc() {
        let evs = vec![
            enter(1, 10, 1, "small.py", 1),
            ops(2, 15, 9, "softmax_f16", 50),
            ret(3, 20, 1),
            enter(4, 30, 2, "big.py", 1),
            ops(5, 35, 10, "gemm_bf16", 500),
            ret(6, 40, 2),
        ];
        let out = compute_dispatch_origins(&evs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].kind, "Matmul");
        assert_eq!(out[0].gpu_ns, 500);
        assert_eq!(out[1].kind, "Softmax");
        assert_eq!(out[1].gpu_ns, 50);
    }

    #[test]
    fn dispatch_origins_distinct_file_line_same_kind_kept_separate() {
        let evs = vec![
            enter(1, 10, 1, "attention.py", 100),
            ops(2, 15, 9, "gemm_bf16", 100),
            ret(3, 20, 1),
            enter(4, 30, 2, "attention.py", 200),
            ops(5, 35, 10, "gemm_bf16", 200),
            ret(6, 40, 2),
        ];
        let out = compute_dispatch_origins(&evs);
        assert_eq!(out.len(), 2);
        let lines: Vec<&str> = out.iter().map(|o| o.file_line.as_str()).collect();
        assert!(lines.contains(&"attention.py:100"));
        assert!(lines.contains(&"attention.py:200"));
    }

    #[test]
    fn dispatch_origins_attributes_cb_arriving_after_async_return() {
        // Repro for issue #38: MLX returns async at t=15, but the CB completes
        // at t=100 (85 ms later, well inside the 500 ms grace window).
        let evs = vec![
            enter(1, 10, 1, "/user/script.py", 17),
            ret_async(2, 15, 1),
            ops(3, 100, 9, "gemm_bf16", 300),
        ];
        let out = compute_dispatch_origins(&evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file_line, "script.py:17");
        assert_eq!(out[0].gpu_ns, 300);
        assert_eq!(out[0].dispatch_count, 1);
    }

    #[test]
    fn dispatch_origins_skips_cb_outside_grace() {
        let evs = vec![
            enter(1, 10, 1, "/user/script.py", 17),
            ret_async(2, 15, 1),
            // 600 ms past return.
            ops(3, 600_000_015, 9, "gemm_bf16", 300),
        ];
        let out = compute_dispatch_origins(&evs);
        assert!(out.is_empty(), "CB past grace must not be attributed");
    }

    fn module_enter(seq: u64, ts: u64, call_id: u64, qualname: &str) -> Event {
        ev(
            seq,
            ts,
            Source::PythonSidecar,
            Payload::ModuleEntered {
                module_call_id: call_id,
                module_def_id: 1,
                qualname: qualname.into(),
                class_name: qualname.into(),
                parent_call_id: None,
                depth: 0,
                fields: Default::default(),
            },
        )
    }

    fn module_ret(seq: u64, ts: u64, call_id: u64) -> Event {
        ev(
            seq,
            ts,
            Source::PythonSidecar,
            Payload::ModuleReturned {
                module_call_id: call_id,
            },
        )
    }

    /// #140: lazy workloads (ERNIE: 2 mx.eval calls) leave nearly every CB
    /// without an eval window. Those CBs fall back to the innermost open
    /// scope/module window, reported as `scope:<qualname>`.
    #[test]
    fn dispatch_origins_falls_back_to_scope_window() {
        let evs = vec![
            module_enter(1, 10, 7, "generate"),
            ops(2, 15, 9, "gemm_bf16", 300),
            module_ret(3, 20, 7),
        ];
        let out = compute_dispatch_origins(&evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, "Matmul");
        assert_eq!(out[0].file_line, "scope:generate");
        assert_eq!(out[0].gpu_ns, 300);
        assert_eq!(out[0].dispatch_count, 1);
    }

    #[test]
    fn dispatch_origins_eval_window_outranks_scope_fallback() {
        // A CB inside BOTH an eval window and a scope window goes to the
        // eval's file:line, never to scope:.
        let evs = vec![
            module_enter(1, 5, 7, "generate"),
            enter(2, 10, 1, "/work/attention.py", 127),
            ops(3, 15, 9, "gemm_bf16", 100),
            ret(4, 20, 1),
            module_ret(5, 25, 7),
        ];
        let out = compute_dispatch_origins(&evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file_line, "attention.py:127");
    }

    #[test]
    fn dispatch_origins_scope_fallback_picks_innermost_window() {
        let evs = vec![
            module_enter(1, 10, 7, "generate"),
            module_enter(2, 12, 8, "TransformerBlock"),
            ops(3, 15, 9, "gemm_bf16", 100),
            module_ret(4, 18, 8),
            module_ret(5, 25, 7),
        ];
        let out = compute_dispatch_origins(&evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file_line, "scope:TransformerBlock");
    }

    #[test]
    fn dispatch_origins_scope_fallback_applies_async_grace() {
        // The CB completes 85 ms after the scope closed — inside the grace.
        let evs = vec![
            module_enter(1, 10, 7, "generate"),
            module_ret(2, 15, 7),
            ops(3, 85_000_015, 9, "gemm_bf16", 300),
        ];
        let out = compute_dispatch_origins(&evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file_line, "scope:generate");
    }

    #[test]
    fn dispatch_origins_cb_outside_every_window_still_dropped() {
        let evs = vec![
            module_enter(1, 10, 7, "generate"),
            module_ret(2, 15, 7),
            // 600 ms past the scope exit — outside the grace.
            ops(3, 600_000_015, 9, "gemm_bf16", 300),
        ];
        let out = compute_dispatch_origins(&evs);
        assert!(out.is_empty());
    }

    #[test]
    fn dispatch_origins_sync_return_does_not_apply_grace() {
        // was_async=false: grace must NOT extend the window. A CB just past
        // t_out should be dropped, preserving existing strict semantics.
        let evs = vec![
            enter(1, 10, 1, "/user/script.py", 17),
            ret(2, 15, 1), // existing helper: was_async=false
            ops(3, 100, 9, "gemm_bf16", 300),
        ];
        let out = compute_dispatch_origins(&evs);
        assert!(
            out.is_empty(),
            "sync return must not extend window via grace"
        );
    }
}

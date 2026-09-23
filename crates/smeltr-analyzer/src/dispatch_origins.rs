//! Attribute kernel dispatches to Python source file:line.
//!
//! Joins each `MlxEvalEntered.stack_frames` (top frame) with the command
//! buffers committed within the eval's window, aggregating per
//! `(kind, file_line)` → `(sum_gpu_ns, count)`.

use crate::op_kinds::resolve_kind;
use crate::windows::{eval_at, eval_windows, scope_windows, EvalWindow, ScopeSweep};
use serde::{Deserialize, Serialize};
use smeltr_core::event::{Event, OpSample};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DispatchOrigin {
    pub kind: String,
    pub file_line: String,
    pub gpu_ns: u64,
    pub dispatch_count: u64,
}

/// Compute per-(kind, file:line) dispatch attribution.
///
/// Each completed command buffer is attributed at its **commit** time, by
/// the same rules as `breakdown`: the eval window picked by
/// [`crate::windows::eval_at`] names the file:line; failing that, the
/// innermost open scope (`scope:<qualname>`). Keying on the time the ops
/// arrived (after completion) instead dropped CBs committed late in a grace
/// tail, and a different overlap rule split CBs between evals (#243).
///
/// Empty if no events carry stack_frames (capture disabled).
pub fn compute_dispatch_origins(events: &[Event]) -> Vec<DispatchOrigin> {
    // Only evals carrying a captured frame can name an origin. Keeping the
    // frame-less ones would let them shadow an enclosing eval that does have
    // one, since `eval_at` takes the latest window covering the commit.
    let evals: Vec<EvalWindow> = eval_windows(events)
        .into_iter()
        .filter(|w| w.top_frame.is_some())
        .collect();
    // #140 scope fallback — same rule as breakdown's step 4.5: lazy
    // workloads barely call mx.eval, so CBs with no eval window are
    // attributed to the innermost scope open at that time, as
    // `scope:<qualname>`.
    let scopes = scope_windows(events);

    // #146: op times clamped to their CB's serialization window.
    let mut cbs: Vec<(u64, Vec<OpSample>)> = crate::breakdown::clamped_command_buffers(events)
        .completed
        .into_iter()
        .filter_map(|cb| cb.ops.map(|ops| (cb.commit_ts, ops)))
        .collect();
    // The scope sweep only moves forward in time.
    cbs.sort_by_key(|(commit_ts, _)| *commit_ts);
    let mut sweep = ScopeSweep::new(&scopes.windows);

    let mut agg: HashMap<(String, String), (u64, u64)> = HashMap::new();
    for (commit_ts, ops) in cbs {
        let eval_frame = eval_at(&evals, commit_ts).and_then(|i| evals[i].top_frame.clone());
        let file_line = match eval_frame {
            Some(frame) => frame,
            None => match sweep.innermost_at(commit_ts) {
                Some(win) => format!("scope:{}", win.qualname),
                None => continue,
            },
        };
        for op in &ops {
            let entry = agg
                .entry((op_kind(op), file_line.clone()))
                .or_insert((0, 0));
            entry.0 += op.gpu_ns;
            entry.1 += op.count as u64;
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
    use smeltr_core::event::{Payload, Source, StackFrame};
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
    fn dispatch_origins_clamps_op_times_to_cb_window() {
        // CB 9: scheduled t=12, completed t=14 -> window 2 ns; its op
        // claims 100 ns (pipelined-encoder over-measure, #146).
        let evs = vec![
            enter(1, 10, 1, "/work/attention.py", 127),
            ev(
                2,
                12,
                Source::MetalHook,
                Payload::MetalCbScheduled {
                    cb_id: 9,
                    queue_id: 1,
                },
            ),
            ev(
                3,
                14,
                Source::MetalHook,
                Payload::MetalCbCompleted {
                    cb_id: 9,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 2,
                },
            ),
            ops(4, 15, 9, "gemm_bf16", 100),
            ret(5, 20, 1),
        ];
        let out = origins(evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].gpu_ns, 2);
    }

    /// The hook always emits `Committed`, `Completed`, then `CbOps`; the
    /// fixtures above mostly write the `CbOps` alone. Supply the missing
    /// lifecycle at the same instant, so each fixture keeps its meaning
    /// now that origins keys on the commit time like breakdown (#243).
    fn with_lifecycles(evs: Vec<Event>) -> Vec<Event> {
        let mut out = Vec::new();
        let mut committed = std::collections::HashSet::new();
        let mut completed = std::collections::HashSet::new();
        for e in evs {
            let cb = match &e.payload {
                Payload::MetalCbCommitted { cb_id, .. } => {
                    committed.insert(*cb_id);
                    None
                }
                Payload::MetalCbCompleted { cb_id, .. } => {
                    completed.insert(*cb_id);
                    (!committed.contains(cb_id)).then_some((*cb_id, false))
                }
                Payload::MetalCbOps { cb_id, .. } => {
                    (!completed.contains(cb_id)).then_some((*cb_id, true))
                }
                _ => None,
            };
            if let Some((cb_id, needs_completion)) = cb {
                if committed.insert(cb_id) {
                    out.push(ev(
                        0,
                        e.ts_mono_ns,
                        Source::MetalHook,
                        Payload::MetalCbCommitted {
                            cb_id,
                            queue_id: 1,
                            queue_depth: 1,
                            label: None,
                        },
                    ));
                }
                if needs_completion {
                    completed.insert(cb_id);
                    out.push(ev(
                        0,
                        e.ts_mono_ns,
                        Source::MetalHook,
                        Payload::MetalCbCompleted {
                            cb_id,
                            queue_id: 1,
                            status: 4,
                            error_code: None,
                            error_domain: None,
                            in_flight_ns: 0,
                        },
                    ));
                }
            }
            out.push(e);
        }
        for (i, e) in out.iter_mut().enumerate() {
            e.seq = i as u64 + 1;
        }
        out
    }

    fn origins(evs: Vec<Event>) -> Vec<DispatchOrigin> {
        compute_dispatch_origins(&with_lifecycles(evs))
    }

    fn committed(seq: u64, ts: u64, cb_id: u64) -> Event {
        ev(
            seq,
            ts,
            Source::MetalHook,
            Payload::MetalCbCommitted {
                cb_id,
                queue_id: 1,
                queue_depth: 1,
                label: None,
            },
        )
    }

    fn completed(seq: u64, ts: u64, cb_id: u64) -> Event {
        ev(
            seq,
            ts,
            Source::MetalHook,
            Payload::MetalCbCompleted {
                cb_id,
                queue_id: 1,
                status: 4,
                error_code: None,
                error_domain: None,
                in_flight_ns: 0,
            },
        )
    }

    /// #243: overlapping async evals — same eval as breakdown.
    #[test]
    fn overlapping_evals_attribute_to_the_latest_like_breakdown() {
        let evs = vec![
            enter(1, 100, 1, "a.py", 1),
            ret_async(2, 105, 1),
            enter(3, 110, 2, "b.py", 1),
            ret_async(4, 115, 2),
            ops(5, 120, 9, "gemm_bf16", 10),
        ];
        let out = origins(evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].file_line, "b.py:1");
    }

    /// #243: a CB is attributed at its commit, like in breakdown — not when
    /// its ops arrive after completion, which may be past the grace tail.
    #[test]
    fn cb_is_attributed_at_commit_not_completion() {
        let grace_end = 15 + crate::windows::ASYNC_GRACE_NS;
        let evs = vec![
            enter(1, 10, 1, "/user/script.py", 17),
            ret_async(2, 15, 1),
            committed(3, grace_end - 1_000, 9),
            completed(4, grace_end + 200_000_000, 9),
            ops(5, grace_end + 200_000_001, 9, "gemm_bf16", 300),
        ];
        let out = compute_dispatch_origins(&evs);
        assert_eq!(out.len(), 1, "CB committed inside the window: {out:?}");
        assert_eq!(out[0].file_line, "script.py:17");
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
        let out = origins(evs);
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
        let out = origins(evs);
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
        let out = origins(evs);
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
        let out = origins(evs);
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
        let out = origins(evs);
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
        let out = origins(evs);
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
        let out = origins(evs);
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
        let out = origins(evs);
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
        let out = origins(evs);
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
        let out = origins(evs);
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
        let out = origins(evs);
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
        let out = origins(evs);
        assert!(
            out.is_empty(),
            "sync return must not extend window via grace"
        );
    }
}

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

    for ev in events {
        match &ev.payload {
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
            Payload::MlxEvalReturned { call_id, .. } => {
                if let Some((t_in, file_line)) = open.remove(call_id) {
                    evals.push(EvalWindow {
                        t_in,
                        t_out: ev.ts_mono_ns,
                        file_line,
                    });
                }
            }
            _ => {}
        }
    }

    let mut agg: HashMap<(String, String), (u64, u64)> = HashMap::new();
    for ev in events {
        if let Payload::MetalCbOps { ops, .. } = &ev.payload {
            let Some(window) = find_window(&evals, ev.ts_mono_ns) else {
                continue;
            };
            for op in ops {
                let kind = op_kind(op);
                let entry = agg
                    .entry((kind, window.file_line.clone()))
                    .or_insert((0, 0));
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
}

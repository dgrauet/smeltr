//! Time windows every attribution pass builds from the same event stream.
//!
//! Four modules used to rebuild these by hand — `breakdown`,
//! `dispatch_origins`, `memory` and `rules::lazy_eval_attribution` — each
//! with its own private `ASYNC_GRACE_NS` and its own comment saying it
//! mirrored one of the others. They had already drifted once: `memory`
//! closed scopes by stack position while the rest matched the call id, which
//! silently misattributed memory whenever a return arrived orphaned or out
//! of order.
//!
//! The async grace is the reason these windows are not simply
//! `[entered, returned]`. MLX 0.31+ schedules GPU work asynchronously:
//! `mx.eval()` returns within ~10 ms of queuing, while the driver thread
//! commits the Metal command buffers up to ~500 ms later. Without extending
//! the window past the return, most command buffers fall outside every
//! window and attribution collapses into `<unscoped>`.

use smeltr_core::event::{Event, Payload};
use std::collections::HashMap;

/// How far past a scope or eval return a command buffer may still commit and
/// count as that window's work.
pub const ASYNC_GRACE_NS: u64 = 500_000_000; // 500 ms

/// One `mx.eval()` call, with its window already widened by the async grace.
#[derive(Debug, Clone)]
pub struct EvalWindow {
    pub t_in: u64,
    /// Return timestamp, plus [`ASYNC_GRACE_NS`] when the eval was async.
    pub t_out: u64,
    /// `seq` of the `MlxEvalEntered` event.
    pub seq: u64,
    /// Module call ids open at the eval, innermost last. Empty means the
    /// eval was made outside any instrumented module.
    pub module_stack: Vec<u64>,
    /// Top Python frame as `file.py:lineno`, when stack capture was on
    /// (`SMELTR_STACK_CAPTURE=1`).
    pub top_frame: Option<String>,
}

/// Paired `MlxEvalEntered`/`MlxEvalReturned` windows, sorted by `t_in`.
///
/// Evals that never returned are dropped: with no return there is no
/// meaningful window end.
pub fn eval_windows(events: &[Event]) -> Vec<EvalWindow> {
    let mut open: HashMap<u64, EvalWindow> = HashMap::new();
    let mut out: Vec<EvalWindow> = Vec::new();
    for ev in events {
        match &ev.payload {
            Payload::MlxEvalEntered {
                call_id,
                module_stack,
                stack_frames,
                ..
            } => {
                open.insert(
                    *call_id,
                    EvalWindow {
                        t_in: ev.ts_mono_ns,
                        t_out: ev.ts_mono_ns,
                        seq: ev.seq,
                        module_stack: module_stack.clone(),
                        top_frame: stack_frames.first().map(|f| {
                            format!("{}:{}", smeltr_core::fmt::basename(&f.filename), f.lineno)
                        }),
                    },
                );
            }
            Payload::MlxEvalReturned {
                call_id, was_async, ..
            } => {
                if let Some(mut w) = open.remove(call_id) {
                    w.t_out = if *was_async {
                        ev.ts_mono_ns.saturating_add(ASYNC_GRACE_NS)
                    } else {
                        ev.ts_mono_ns
                    };
                    out.push(w);
                }
            }
            _ => {}
        }
    }
    out.sort_by_key(|w| w.t_in);
    out
}

/// One `ModuleEntered`/`ModuleReturned` pair — a `smeltr.scope(...)` block or
/// an auto-wrapped `mlx.nn.Module.__call__`.
#[derive(Debug, Clone)]
pub struct ScopeWindow {
    pub t_in: u64,
    /// Return timestamp, or the session's last event when the call never
    /// returned (aborted run). Consumers add [`ASYNC_GRACE_NS`] themselves,
    /// since only some of them want the tail.
    pub t_out: u64,
    pub call_id: u64,
    pub qualname: String,
}

pub struct ScopeWindows {
    /// Sorted by `t_in`.
    pub windows: Vec<ScopeWindow>,
    /// Returns naming a call that was never entered, plus calls still open
    /// at the end of the session. Surfaced as a breakdown diagnostic.
    pub malformed_returns: u64,
}

/// Build every module window in the stream.
///
/// A return closes the call it names, wherever that call sits in the open
/// set — never simply the most recent one. Returns for unknown calls are
/// counted and otherwise ignored, so a lost `Entered` cannot evict a live
/// scope.
pub fn scope_windows(events: &[Event]) -> ScopeWindows {
    let last_event_ts = events.last().map(|e| e.ts_mono_ns).unwrap_or(0);
    let mut windows: Vec<ScopeWindow> = Vec::new();
    let mut index: HashMap<u64, usize> = HashMap::new();
    let mut open: Vec<u64> = Vec::new();
    let mut malformed_returns: u64 = 0;

    for ev in events {
        match &ev.payload {
            Payload::ModuleEntered {
                module_call_id,
                qualname,
                ..
            } => {
                index.insert(*module_call_id, windows.len());
                open.push(*module_call_id);
                windows.push(ScopeWindow {
                    t_in: ev.ts_mono_ns,
                    // Closed on ModuleReturned; a never-returned call stays
                    // open until the end of the session.
                    t_out: last_event_ts,
                    call_id: *module_call_id,
                    qualname: qualname.clone(),
                });
            }
            Payload::ModuleReturned { module_call_id } => {
                match open.iter().rposition(|c| c == module_call_id) {
                    Some(pos) => {
                        open.remove(pos);
                        if let Some(&i) = index.get(module_call_id) {
                            windows[i].t_out = ev.ts_mono_ns;
                        }
                    }
                    // A second return for an already-closed call is
                    // redundant, not malformed; one for a call never seen
                    // means its Entered was lost.
                    None if !index.contains_key(module_call_id) => malformed_returns += 1,
                    None => {}
                }
            }
            _ => {}
        }
    }
    malformed_returns += open.len() as u64;
    windows.sort_by_key(|w| w.t_in);
    ScopeWindows {
        windows,
        malformed_returns,
    }
}

/// Walks chronologically ordered timestamps over `t_in`-sorted windows,
/// reporting the innermost scope open at each one.
///
/// Module calls nest on the Python side, so inner calls close before outer
/// ones and expired windows pop off the top of the stack. Timestamps must be
/// non-decreasing across calls — the sweep never rewinds.
pub struct ScopeSweep<'a> {
    windows: &'a [ScopeWindow],
    next: usize,
    stack: Vec<&'a ScopeWindow>,
}

impl<'a> ScopeSweep<'a> {
    pub fn new(windows: &'a [ScopeWindow]) -> Self {
        Self {
            windows,
            next: 0,
            stack: Vec::new(),
        }
    }

    /// Innermost scope covering `ts`, allowing each window the async grace
    /// tail. `None` when `ts` falls outside every scope.
    pub fn innermost_at(&mut self, ts: u64) -> Option<&'a ScopeWindow> {
        while self.next < self.windows.len() && self.windows[self.next].t_in <= ts {
            self.stack.push(&self.windows[self.next]);
            self.next += 1;
        }
        while let Some(top) = self.stack.last() {
            if top.t_out.saturating_add(ASYNC_GRACE_NS) < ts {
                self.stack.pop();
            } else {
                break;
            }
        }
        self.stack.last().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Source, StackFrame};
    use uuid::Uuid;

    fn ev(seq: u64, ts: u64, payload: Payload) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source: Source::PythonSidecar,
            pid: None,
            seq,
            payload,
        }
    }

    fn entered(seq: u64, ts: u64, id: u64, qualname: &str) -> Event {
        ev(
            seq,
            ts,
            Payload::ModuleEntered {
                module_call_id: id,
                module_def_id: 0,
                qualname: qualname.into(),
                class_name: "Scope".into(),
                parent_call_id: None,
                depth: 0,
                fields: Default::default(),
            },
        )
    }

    fn returned(seq: u64, ts: u64, id: u64) -> Event {
        ev(seq, ts, Payload::ModuleReturned { module_call_id: id })
    }

    fn eval_in(seq: u64, ts: u64, id: u64, stack: Vec<u64>, frames: Vec<StackFrame>) -> Event {
        ev(
            seq,
            ts,
            Payload::MlxEvalEntered {
                call_id: id,
                array_count: 1,
                stream: "gpu".into(),
                module_stack: stack,
                stack_frames: frames,
            },
        )
    }

    fn eval_out(seq: u64, ts: u64, id: u64, was_async: bool) -> Event {
        ev(
            seq,
            ts,
            Payload::MlxEvalReturned {
                call_id: id,
                duration_ns: 0,
                was_async,
            },
        )
    }

    #[test]
    fn async_eval_window_gets_the_grace_tail() {
        let evs = vec![
            eval_in(1, 100, 1, vec![], vec![]),
            eval_out(2, 200, 1, true),
        ];
        let w = eval_windows(&evs);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].t_in, 100);
        assert_eq!(w[0].t_out, 200 + ASYNC_GRACE_NS);
    }

    #[test]
    fn sync_eval_window_ends_at_the_return() {
        let evs = vec![
            eval_in(1, 100, 1, vec![], vec![]),
            eval_out(2, 200, 1, false),
        ];
        assert_eq!(eval_windows(&evs)[0].t_out, 200);
    }

    #[test]
    fn unreturned_eval_is_dropped() {
        assert!(eval_windows(&[eval_in(1, 100, 1, vec![], vec![])]).is_empty());
    }

    #[test]
    fn eval_window_carries_stack_and_top_frame() {
        let frames = vec![StackFrame {
            filename: "/src/pipeline/denoise.py".into(),
            lineno: 42,
            funcname: "step".into(),
        }];
        let evs = vec![
            eval_in(1, 100, 1, vec![7], frames),
            eval_out(2, 200, 1, false),
        ];
        let w = &eval_windows(&evs)[0];
        assert_eq!(w.module_stack, vec![7]);
        assert_eq!(w.top_frame.as_deref(), Some("denoise.py:42"));
    }

    #[test]
    fn scope_window_closes_at_its_return() {
        let evs = vec![entered(1, 10, 1, "a"), returned(2, 90, 1)];
        let r = scope_windows(&evs);
        assert_eq!(r.windows.len(), 1);
        assert_eq!((r.windows[0].t_in, r.windows[0].t_out), (10, 90));
        assert_eq!(r.malformed_returns, 0);
    }

    #[test]
    fn unreturned_scope_runs_to_the_last_event() {
        let evs = vec![entered(1, 10, 1, "a"), entered(2, 20, 2, "b")];
        let r = scope_windows(&evs);
        assert_eq!(r.windows[0].t_out, 20);
        assert_eq!(r.malformed_returns, 2, "both calls left open");
    }

    #[test]
    fn out_of_order_return_closes_the_call_it_names() {
        let evs = vec![
            entered(1, 10, 1, "outer"),
            entered(2, 20, 2, "inner"),
            returned(3, 30, 1),
            returned(4, 40, 2),
        ];
        let r = scope_windows(&evs);
        let outer = r.windows.iter().find(|w| w.qualname == "outer").unwrap();
        let inner = r.windows.iter().find(|w| w.qualname == "inner").unwrap();
        assert_eq!(outer.t_out, 30);
        assert_eq!(inner.t_out, 40);
        assert_eq!(r.malformed_returns, 0);
    }

    #[test]
    fn orphan_return_is_counted_and_closes_nothing() {
        let evs = vec![
            entered(1, 10, 1, "a"),
            returned(2, 20, 99),
            returned(3, 30, 1),
        ];
        let r = scope_windows(&evs);
        assert_eq!(r.windows[0].t_out, 30, "the live scope survived");
        assert_eq!(r.malformed_returns, 1);
    }

    #[test]
    fn sweep_reports_the_innermost_open_scope() {
        let evs = vec![
            entered(1, 10, 1, "outer"),
            entered(2, 20, 2, "inner"),
            returned(3, 30, 2),
            returned(4, 40, 1),
        ];
        let r = scope_windows(&evs);
        let mut sweep = ScopeSweep::new(&r.windows);
        assert_eq!(
            sweep.innermost_at(15).map(|w| &w.qualname[..]),
            Some("outer")
        );
        assert_eq!(
            sweep.innermost_at(25).map(|w| &w.qualname[..]),
            Some("inner")
        );
        // Within the grace tail, `inner` still owns the work it queued.
        assert_eq!(
            sweep.innermost_at(35).map(|w| &w.qualname[..]),
            Some("inner")
        );
        // Past both grace tails, nothing is open.
        assert_eq!(
            sweep
                .innermost_at(40 + ASYNC_GRACE_NS + 1)
                .map(|w| &w.qualname[..]),
            None
        );
    }

    #[test]
    fn sweep_returns_none_before_any_scope_opens() {
        let evs = vec![entered(1, 100, 1, "a"), returned(2, 200, 1)];
        let r = scope_windows(&evs);
        assert!(ScopeSweep::new(&r.windows).innermost_at(50).is_none());
    }
}

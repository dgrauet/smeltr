//! Surfaces the fully-lazy attribution gap (#163) as an explicit finding.
//!
//! On Matrix-Game-3-mlx (2026-07-19) the pipeline evaluates each step's whole
//! graph with a single `mx.eval(latents)` at pipeline level: module
//! instrumentation works (`ModuleEntered/Returned` are emitted), but the eval
//! window that contains every Metal CB has an EMPTY `module_stack`, so the
//! breakdown silently reports ~100 % `<unscoped>`. Attribution through the
//! lazy graph itself would need MLX-side node tagging that does not exist;
//! what smeltr can do is detect the pattern and say plainly how to fix the
//! attribution (`smeltr.scope()`), instead of leaving `<unscoped>` to be
//! misread as a smeltr bug.

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};
use std::collections::HashMap;

/// Minimum share (percent of total attributed GPU time) landing in
/// empty-module-stack eval windows before the gap is reported.
pub const LAZY_GAP_THRESHOLD_PCT: u64 = 50;

/// Same async-scheduling grace as `breakdown::compute` (see #131/#136).
const ASYNC_GRACE_NS: u64 = 500_000_000;

/// Detected lazy-eval attribution gap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LazyEvalGap {
    /// GPU ns attributed to eval windows whose `module_stack` was empty.
    pub gap_gpu_ns: u64,
    /// Total GPU ns considered (all completed CBs).
    pub total_gpu_ns: u64,
    /// Number of `ModuleEntered` events in the session (instrumentation
    /// present but bypassed by the lazy graph when > 0).
    pub module_call_count: u64,
    /// Number of empty-stack evals that received GPU time.
    pub lazy_eval_count: u64,
    /// seq / ts of the first empty-stack eval that received GPU time.
    pub first_seq: u64,
    pub first_ts_mono_ns: u64,
}

impl LazyEvalGap {
    pub fn gap_pct(&self) -> u64 {
        (self.gap_gpu_ns * 100)
            .checked_div(self.total_gpu_ns)
            .unwrap_or(0)
    }

    /// Canonical human-readable explanation, shared by the CLI notice, the
    /// analyze finding detail and the MCP response.
    pub fn advice(&self) -> String {
        let cause = if self.module_call_count > 0 {
            "module instrumentation is active, but the graph is evaluated \
             lazily by mx.eval() calls made outside any module forward \
             (typically a single eval per step at pipeline level)"
        } else {
            "the recorded process calls mx.eval() with no module or scope \
             instrumentation around the work"
        };
        format!(
            "{}% of GPU time cannot be attributed to modules: {cause}, so it \
             is reported as <unscoped>. Add smeltr.scope(\"...\") around \
             pipeline stages for semantic attribution, or record with \
             SMELTR_STACK_CAPTURE=1 and use `smeltr origins` for file:line \
             attribution.",
            self.gap_pct()
        )
    }
}

/// Detects the lazy-eval attribution gap. Mirrors `breakdown::compute`'s
/// CB → eval-window precedence (commit-ts containment, async grace, op-sum
/// GPU ns) without building the module tree. Returns `Some` only when the
/// empty-stack share crosses [`LAZY_GAP_THRESHOLD_PCT`].
pub fn detect(events: &[Event]) -> Option<LazyEvalGap> {
    // 1. Eval intervals (paired by call_id, async grace on t_out), flagged
    //    empty when their module_stack was empty at eval entry.
    struct EvalInterval {
        t_in: u64,
        t_out: u64,
        empty_stack: bool,
        seq: u64,
        ts: u64,
    }
    let mut entered: HashMap<u64, (u64, bool, u64, u64)> = HashMap::new();
    let mut intervals: Vec<EvalInterval> = Vec::new();
    // 2. Module instrumentation presence (drives the advice wording only —
    //    CBs outside eval windows are attributable via the #131 scope
    //    fallback and never count toward the gap).
    let mut module_call_count: u64 = 0;
    // 3. Completed CBs: (commit_ts, gpu_ns) with the same GPU-ns choice as
    //    breakdown::compute (op sum; 0 for op-less CBs once any CB carried
    //    ops; in_flight fallback only when op capture was off).
    let mut cb_commit_ts: HashMap<u64, u64> = HashMap::new();
    let mut cb_completed: Vec<(u64, u64, u64)> = Vec::new(); // (cb_id, commit, in_flight)
    let mut last_completed_idx: HashMap<u64, usize> = HashMap::new();
    let mut cb_ops_ns: HashMap<usize, u64> = HashMap::new();
    let mut seen_any_cb_ops = false;

    for ev in events {
        match &ev.payload {
            Payload::MlxEvalEntered {
                call_id,
                module_stack,
                ..
            } => {
                entered.insert(
                    *call_id,
                    (
                        ev.ts_mono_ns,
                        module_stack.is_empty(),
                        ev.seq,
                        ev.ts_mono_ns,
                    ),
                );
            }
            Payload::MlxEvalReturned {
                call_id, was_async, ..
            } => {
                if let Some((t_in, empty_stack, seq, ts)) = entered.remove(call_id) {
                    let t_out = if *was_async {
                        ev.ts_mono_ns.saturating_add(ASYNC_GRACE_NS)
                    } else {
                        ev.ts_mono_ns
                    };
                    intervals.push(EvalInterval {
                        t_in,
                        t_out,
                        empty_stack,
                        seq,
                        ts,
                    });
                }
            }
            Payload::ModuleEntered { .. } => {
                module_call_count += 1;
            }
            Payload::MetalCbCommitted { cb_id, .. } => {
                cb_commit_ts.insert(*cb_id, ev.ts_mono_ns);
            }
            Payload::MetalCbCompleted {
                cb_id,
                in_flight_ns,
                ..
            } => {
                if let Some(commit_ts) = cb_commit_ts.remove(cb_id) {
                    last_completed_idx.insert(*cb_id, cb_completed.len());
                    cb_completed.push((*cb_id, commit_ts, *in_flight_ns));
                }
            }
            Payload::MetalCbOps { cb_id, ops } => {
                seen_any_cb_ops = true;
                if let Some(&i) = last_completed_idx.get(cb_id) {
                    cb_ops_ns.insert(i, ops.iter().map(|o| o.gpu_ns).sum());
                }
            }
            _ => {}
        }
    }
    intervals.sort_by_key(|e| e.t_in);

    // 4. Bucket each CB by the same precedence as breakdown::compute:
    //    containing eval window first, module/scope window second.
    let mut gap_gpu_ns: u64 = 0;
    let mut total_gpu_ns: u64 = 0;
    let mut lazy_evals: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut first: Option<(u64, u64)> = None;
    for (i, (_cb_id, commit_ts, in_flight_ns)) in cb_completed.iter().enumerate() {
        let ns = match cb_ops_ns.get(&i) {
            Some(ns) => *ns,
            None if seen_any_cb_ops => 0,
            None => *in_flight_ns,
        };
        total_gpu_ns += ns;
        let hit = intervals
            .iter()
            .enumerate()
            .find(|(_, e)| e.t_in <= *commit_ts && *commit_ts <= e.t_out);
        if let Some((idx, interval)) = hit {
            if interval.empty_stack && ns > 0 {
                gap_gpu_ns += ns;
                lazy_evals.insert(idx);
                let evidence = (interval.seq, interval.ts);
                first = Some(first.map_or(evidence, |f| f.min(evidence)));
            }
        }
    }

    let (first_seq, first_ts_mono_ns) = first?;
    let gap = LazyEvalGap {
        gap_gpu_ns,
        total_gpu_ns,
        module_call_count,
        lazy_eval_count: lazy_evals.len() as u64,
        first_seq,
        first_ts_mono_ns,
    };
    (gap.gap_pct() >= LAZY_GAP_THRESHOLD_PCT).then_some(gap)
}

pub struct LazyEvalAttributionRule;

impl Rule for LazyEvalAttributionRule {
    fn name(&self) -> &'static str {
        "lazy_eval_attribution"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        let Some(gap) = detect(events) else {
            return Vec::new();
        };
        vec![Finding::new(
            Severity::Warning,
            Category::ContributingFactor,
            format!(
                "module attribution gap: {}% of GPU time evaluated outside \
                 module windows (lazy graph)",
                gap.gap_pct()
            ),
        )
        .with_detail(gap.advice())
        .with_evidence(EvidenceRef {
            seq: gap.first_seq,
            ts_mono_ns: gap.first_ts_mono_ns,
            description: format!(
                "first empty-module-stack mx.eval that received GPU time \
                 ({} such eval(s), {} ModuleEntered in session)",
                gap.lazy_eval_count, gap.module_call_count
            ),
        })]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::test_helpers::ev;
    use smeltr_core::event::{OpSample, Source};

    fn module_entered(ts: u64, call_id: u64) -> Event {
        ev(
            ts,
            Source::PythonSidecar,
            Payload::ModuleEntered {
                module_call_id: call_id,
                module_def_id: 1,
                qualname: format!("m{call_id}"),
                class_name: "M".into(),
                parent_call_id: None,
                depth: 0,
                fields: Default::default(),
            },
        )
    }

    fn module_returned(ts: u64, call_id: u64) -> Event {
        ev(
            ts,
            Source::PythonSidecar,
            Payload::ModuleReturned {
                module_call_id: call_id,
            },
        )
    }

    fn eval_entered(ts: u64, call_id: u64, stack: Vec<u64>) -> Event {
        ev(
            ts,
            Source::PythonSidecar,
            Payload::MlxEvalEntered {
                call_id,
                array_count: 1,
                stream: "gpu".into(),
                module_stack: stack,
                stack_frames: vec![],
            },
        )
    }

    fn eval_returned(ts: u64, call_id: u64) -> Event {
        ev(
            ts,
            Source::PythonSidecar,
            Payload::MlxEvalReturned {
                call_id,
                duration_ns: 1,
                was_async: false,
            },
        )
    }

    fn cb(commit_ts: u64, complete_ts: u64, cb_id: u64, gpu_ns: u64) -> Vec<Event> {
        vec![
            ev(
                commit_ts,
                Source::MetalHook,
                Payload::MetalCbCommitted {
                    cb_id,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
            ),
            ev(
                complete_ts,
                Source::MetalHook,
                Payload::MetalCbCompleted {
                    cb_id,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: gpu_ns,
                },
            ),
            ev(
                complete_ts + 1,
                Source::MetalHook,
                Payload::MetalCbOps {
                    cb_id,
                    ops: vec![OpSample {
                        name: format!("K_{cb_id}"),
                        symbol: None,
                        gpu_ns,
                        count: 1,
                    }],
                },
            ),
        ]
    }

    /// The #163 shape: module windows open and close BEFORE a single
    /// pipeline-level mx.eval (empty module_stack) that contains all CBs.
    fn lazy_session() -> Vec<Event> {
        let mut events = vec![
            module_entered(100, 1),
            module_returned(200, 1),
            module_entered(300, 2),
            module_returned(400, 2),
            // pipeline-level eval, outside any module forward
            eval_entered(1_000_000_000, 7, vec![]),
        ];
        events.extend(cb(2_000_000_000, 3_000_000_000, 10, 90_000));
        events.extend(cb(4_000_000_000, 5_000_000_000, 11, 90_000));
        events.push(eval_returned(6_000_000_000, 7));
        events.sort_by_key(|e| e.ts_mono_ns);
        events
    }

    #[test]
    fn detects_full_lazy_gap() {
        let gap = detect(&lazy_session()).expect("gap should be detected");
        assert_eq!(gap.gap_gpu_ns, 180_000);
        assert_eq!(gap.total_gpu_ns, 180_000);
        assert_eq!(gap.gap_pct(), 100);
        assert_eq!(gap.module_call_count, 2);
        assert_eq!(gap.lazy_eval_count, 1);
    }

    #[test]
    fn no_gap_when_evals_carry_module_stack() {
        let mut events = vec![module_entered(100, 1), eval_entered(200, 7, vec![1])];
        events.extend(cb(300, 400, 10, 90_000));
        events.push(eval_returned(500, 7));
        events.push(module_returned(600, 1));
        events.sort_by_key(|e| e.ts_mono_ns);
        assert!(detect(&events).is_none());
    }

    /// CBs landing in module/scope windows via the #131 fallback are
    /// attributed fine — a small empty-stack eval must stay under threshold.
    #[test]
    fn below_threshold_share_is_not_reported() {
        let mut events = vec![module_entered(100, 1)];
        // 90% of GPU time inside the module window, no eval window.
        events.extend(cb(200, 300, 10, 900_000));
        events.push(module_returned(1_000, 1));
        // 10% in a later empty-stack eval.
        events.push(eval_entered(600_000_000_000, 7, vec![]));
        events.extend(cb(600_000_000_100, 600_000_000_200, 11, 100_000));
        events.push(eval_returned(600_000_001_000, 7));
        events.sort_by_key(|e| e.ts_mono_ns);
        assert!(detect(&events).is_none());
    }

    #[test]
    fn empty_or_gpu_less_sessions_are_ignored() {
        assert!(detect(&[]).is_none());
        // Modules + empty-stack eval but no CB at all.
        let events = vec![
            module_entered(100, 1),
            module_returned(200, 1),
            eval_entered(300, 7, vec![]),
            eval_returned(400, 7),
        ];
        assert!(detect(&events).is_none());
    }

    #[test]
    fn rule_emits_actionable_warning() {
        let findings = LazyEvalAttributionRule.check(&lazy_session());
        assert_eq!(findings.len(), 1, "got: {findings:#?}");
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warning);
        assert!(f.title.contains("100%"), "title: {}", f.title);
        assert!(f.detail.contains("smeltr.scope"), "detail: {}", f.detail);
        assert!(f.detail.contains("smeltr origins"), "detail: {}", f.detail);
        assert_eq!(f.evidence.len(), 1);
    }

    /// Without module instrumentation at all, the advice must not claim
    /// instrumentation is active.
    #[test]
    fn advice_distinguishes_uninstrumented_sessions() {
        let mut events = vec![eval_entered(1_000, 7, vec![])];
        events.extend(cb(2_000, 3_000, 10, 90_000));
        events.push(eval_returned(4_000, 7));
        events.sort_by_key(|e| e.ts_mono_ns);
        let gap = detect(&events).expect("gap should be detected");
        assert_eq!(gap.module_call_count, 0);
        assert!(gap.advice().contains("no module or scope instrumentation"));
        let gap_instr = detect(&lazy_session()).unwrap();
        assert!(gap_instr.advice().contains("instrumentation is active"));
    }
}

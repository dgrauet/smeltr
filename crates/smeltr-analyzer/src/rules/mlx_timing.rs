//! Surfaces MLX eval timing relative to the first Metal failure.

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

pub struct MlxTimingRule;

impl Rule for MlxTimingRule {
    fn name(&self) -> &'static str {
        "mlx_timing"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        let crash_ts = events.iter().find_map(|e| match &e.payload {
            Payload::MetalCbCompleted {
                error_code: Some(c),
                ..
            } if *c != 0 => Some(e.ts_mono_ns),
            _ => None,
        });

        struct Rec {
            call_id: u64,
            duration_ns: u64,
            was_async: bool,
            entered_ts: Option<u64>,
            returned_ts: u64,
            seq: u64,
        }
        let mut recs: Vec<Rec> = Vec::new();
        let mut last_entered: Option<(u64, u64)> = None;
        for ev in events {
            match &ev.payload {
                Payload::MlxEvalEntered { call_id, .. } => {
                    last_entered = Some((*call_id, ev.ts_mono_ns));
                }
                Payload::MlxEvalReturned {
                    call_id,
                    duration_ns,
                    was_async,
                } => {
                    recs.push(Rec {
                        call_id: *call_id,
                        duration_ns: *duration_ns,
                        was_async: *was_async,
                        entered_ts: last_entered
                            .filter(|(cid, _)| *cid == *call_id)
                            .map(|(_, ts)| ts),
                        returned_ts: ev.ts_mono_ns,
                        seq: ev.seq,
                    });
                }
                _ => {}
            }
        }
        if recs.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::new();

        // Per-eval findings only where they carry forensic value: the last
        // few evals preceding a Metal failure. A real session has hundreds
        // of evals — one Info finding each flooded the report (695 findings,
        // 140 KB) and buried the actionable Warnings (#115).
        const CRASH_ADJACENT_EVALS: usize = 5;
        if let Some(crash) = crash_ts {
            let mut before: Vec<&Rec> = recs.iter().filter(|r| r.returned_ts <= crash).collect();
            before.sort_by_key(|r| r.returned_ts);
            let skip = before.len().saturating_sub(CRASH_ADJACENT_EVALS);
            for r in before.into_iter().skip(skip) {
                let title = format!(
                    "mx.eval call_id={} returned ({}{}ms)",
                    r.call_id,
                    if r.was_async { "async, " } else { "sync, " },
                    r.duration_ns / 1_000_000
                );
                let ret_to_crash = (crash - r.returned_ts) as f64 / 1e9;
                let detail = match r.entered_ts {
                    Some(entered) => format!(
                        "entered {:.2}s before crash, returned {:.2}s before crash",
                        (crash - entered) as f64 / 1e9,
                        ret_to_crash
                    ),
                    None => format!("returned {ret_to_crash:.2}s before crash"),
                };
                out.push(
                    Finding::new(Severity::Info, Category::Timing, title)
                        .with_detail(detail)
                        .with_evidence(EvidenceRef {
                            seq: r.seq,
                            ts_mono_ns: r.returned_ts,
                            description: format!("MlxEvalReturned call_id={}", r.call_id),
                        }),
                );
            }
        }

        // Aggregate: one finding for the whole session.
        let mut sorted: Vec<&Rec> = recs.iter().collect();
        sorted.sort_by_key(|r| r.duration_ns);
        let pct = |p: f64| -> u64 {
            let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
            sorted[idx].duration_ns
        };
        let n_async = recs.iter().filter(|r| r.was_async).count();
        let n_sync = recs.len() - n_async;
        let max = sorted[sorted.len() - 1];
        let title = format!(
            "mx.eval timing: {} calls ({n_sync} sync, {n_async} async) — p50 {}ms, p95 {}ms, max {}ms",
            recs.len(),
            pct(0.5) / 1_000_000,
            pct(0.95) / 1_000_000,
            max.duration_ns / 1_000_000,
        );
        let slowest: Vec<String> = sorted
            .iter()
            .rev()
            .take(5)
            .map(|r| {
                format!(
                    "call_id={} {}ms ({})",
                    r.call_id,
                    r.duration_ns / 1_000_000,
                    if r.was_async { "async" } else { "sync" }
                )
            })
            .collect();
        out.push(
            Finding::new(Severity::Info, Category::Timing, title)
                .with_detail(format!("slowest: {}", slowest.join(", ")))
                .with_evidence(EvidenceRef {
                    seq: max.seq,
                    ts_mono_ns: max.returned_ts,
                    description: format!("slowest MlxEvalReturned call_id={}", max.call_id),
                }),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::test_helpers::ev;
    use smeltr_core::event::Source;

    fn eval_pair(call_id: u64, t_in: u64, dur_ns: u64, was_async: bool) -> Vec<Event> {
        vec![
            ev(
                t_in,
                Source::PythonSidecar,
                Payload::MlxEvalEntered {
                    call_id,
                    array_count: 1,
                    stream: "gpu".into(),
                    module_stack: Vec::new(),
                    stack_frames: vec![],
                },
            ),
            ev(
                t_in + dur_ns,
                Source::PythonSidecar,
                Payload::MlxEvalReturned {
                    call_id,
                    duration_ns: dur_ns,
                    was_async,
                },
            ),
        ]
    }

    /// #115: a real session has hundreds of evals; without a crash they
    /// must collapse into ONE aggregate finding (a 695-finding report blew
    /// past MCP client limits), with percentiles and the slowest calls.
    #[test]
    fn many_evals_without_crash_aggregate_into_one_finding() {
        let mut events = Vec::new();
        for i in 0..100u64 {
            // call_id 99 is the slowest (990 ms), then 98, ...
            events.extend(eval_pair(
                i,
                i * 2_000_000_000,
                (i + 1) * 10_000_000,
                i % 2 == 0,
            ));
        }
        let findings = MlxTimingRule.check(&events);
        assert_eq!(findings.len(), 1, "got {} findings", findings.len());
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Info);
        assert!(f.title.contains("100"), "title: {}", f.title);
        assert!(f.title.contains("p95"), "title: {}", f.title);
        assert!(
            f.detail.contains("call_id=99"),
            "slowest call must be listed: {}",
            f.detail
        );
    }

    /// Near a crash the per-eval forensic detail keeps its value: the last
    /// few evals before the crash stay as individual findings (capped),
    /// alongside the aggregate.
    #[test]
    fn crash_keeps_individual_findings_for_last_evals_only() {
        let mut events = Vec::new();
        for i in 0..50u64 {
            events.extend(eval_pair(i, i * 1_000_000_000, 100_000_000, false));
        }
        events.push(ev(
            60_000_000_000,
            Source::MetalHook,
            Payload::MetalCbCompleted {
                cb_id: 1,
                queue_id: 1,
                status: 4,
                error_code: Some(14),
                error_domain: Some("IOGPU".into()),
                in_flight_ns: 1,
            },
        ));
        let findings = MlxTimingRule.check(&events);
        let per_eval: Vec<_> = findings
            .iter()
            .filter(|f| f.detail.contains("before crash"))
            .collect();
        assert!(
            per_eval.len() == 5,
            "expected 5 crash-adjacent per-eval findings, got {}",
            per_eval.len()
        );
        // The 5 closest to the crash are call_ids 45..49.
        assert!(per_eval.iter().any(|f| f.title.contains("call_id=49")));
        assert!(!per_eval.iter().any(|f| f.title.contains("call_id=44")));
        assert!(
            findings.iter().any(|f| f.title.contains("50")),
            "aggregate must still be present"
        );
    }

    #[test]
    fn pairs_eval_with_crash() {
        let events = vec![
            ev(
                0,
                Source::PythonSidecar,
                Payload::MlxEvalEntered {
                    call_id: 1,
                    array_count: 3,
                    stream: "gpu".into(),
                    module_stack: Vec::new(),
                    stack_frames: vec![],
                },
            ),
            ev(
                100_000_000,
                Source::PythonSidecar,
                Payload::MlxEvalReturned {
                    call_id: 1,
                    duration_ns: 100_000_000,
                    was_async: true,
                },
            ),
            ev(
                5_000_000_000,
                Source::MetalHook,
                Payload::MetalCbCompleted {
                    cb_id: 1,
                    queue_id: 1,
                    status: 4,
                    error_code: Some(14),
                    error_domain: Some("IOGPU".into()),
                    in_flight_ns: 1,
                },
            ),
        ];
        let findings = MlxTimingRule.check(&events);
        assert_eq!(findings.len(), 2, "crash-adjacent per-eval + aggregate");
        assert!(findings[0].detail.contains("before crash"));
    }
}

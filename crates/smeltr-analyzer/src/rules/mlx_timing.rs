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
        let mut out = Vec::new();
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
                    let pair_ts = last_entered
                        .filter(|(cid, _)| *cid == *call_id)
                        .map(|(_, ts)| ts);
                    let title = format!(
                        "mx.eval call_id={call_id} returned ({}{}ms)",
                        if *was_async { "async, " } else { "sync, " },
                        duration_ns / 1_000_000
                    );
                    let detail = match (pair_ts, crash_ts) {
                        (Some(entered), Some(crash)) if ev.ts_mono_ns <= crash => {
                            let ret_to_crash = (crash - ev.ts_mono_ns) as f64 / 1e9;
                            let entered_to_crash = (crash - entered) as f64 / 1e9;
                            format!(
                                "entered {:.2}s before crash, returned {:.2}s before crash",
                                entered_to_crash, ret_to_crash
                            )
                        }
                        _ => String::new(),
                    };
                    let mut f = Finding::new(Severity::Info, Category::Timing, title);
                    if !detail.is_empty() {
                        f = f.with_detail(detail);
                    }
                    f = f.with_evidence(EvidenceRef {
                        seq: ev.seq,
                        ts_mono_ns: ev.ts_mono_ns,
                        description: format!("MlxEvalReturned call_id={call_id}"),
                    });
                    out.push(f);
                }
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::test_helpers::ev;
    use smeltr_core::event::Source;

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
        assert_eq!(findings.len(), 1);
        assert!(findings[0].detail.contains("before crash"));
    }
}

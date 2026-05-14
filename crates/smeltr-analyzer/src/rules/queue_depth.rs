//! Reports the maximum command-buffer queue depth observed in the 5 seconds
//! before any MetalCbCompleted error.

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

pub struct QueueDepthRule;

const WINDOW_NS: u64 = 5_000_000_000;

impl Rule for QueueDepthRule {
    fn name(&self) -> &'static str {
        "queue_depth"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        let crash = events.iter().find(|e| {
            matches!(
                &e.payload,
                Payload::MetalCbCompleted { error_code: Some(c), .. } if *c != 0
            )
        });
        let Some(crash_ev) = crash else {
            return Vec::new();
        };
        let crash_ts = crash_ev.ts_mono_ns;
        let from = crash_ts.saturating_sub(WINDOW_NS);

        let mut max_depth = 0u32;
        let mut max_at = crash_ts;
        let mut max_cb = 0u64;
        for e in events {
            if e.ts_mono_ns < from || e.ts_mono_ns > crash_ts {
                continue;
            }
            if let Payload::MetalCbCommitted {
                queue_depth, cb_id, ..
            } = &e.payload
            {
                if *queue_depth > max_depth {
                    max_depth = *queue_depth;
                    max_at = e.ts_mono_ns;
                    max_cb = *cb_id;
                }
            }
        }
        if max_depth == 0 {
            return Vec::new();
        }
        let secs_before = (crash_ts - max_at) as f64 / 1_000_000_000.0;
        let title = format!("Queue depth peaked at {max_depth} CBs in-flight");
        let detail = format!(
            "Observed at cb_id={max_cb}, {:.2}s before the failing command buffer.",
            secs_before
        );
        let evidence = EvidenceRef {
            seq: crash_ev.seq,
            ts_mono_ns: crash_ev.ts_mono_ns,
            description: "MetalCbCompleted (failure marker)".into(),
        };
        vec![
            Finding::new(Severity::Warning, Category::ContributingFactor, title)
                .with_detail(detail)
                .with_evidence(evidence),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::test_helpers::ev;
    use smeltr_core::event::Source;

    #[test]
    fn reports_max_queue_depth_before_crash() {
        let events = vec![
            ev(
                1_000_000_000,
                Source::MetalHook,
                Payload::MetalCbCommitted {
                    cb_id: 1,
                    queue_id: 1,
                    queue_depth: 5,
                    label: None,
                },
            ),
            ev(
                2_000_000_000,
                Source::MetalHook,
                Payload::MetalCbCommitted {
                    cb_id: 2,
                    queue_id: 1,
                    queue_depth: 23,
                    label: None,
                },
            ),
            ev(
                3_000_000_000,
                Source::MetalHook,
                Payload::MetalCbCompleted {
                    cb_id: 2,
                    queue_id: 1,
                    status: 4,
                    error_code: Some(14),
                    error_domain: Some("IOGPU".into()),
                    in_flight_ns: 1,
                },
            ),
        ];
        let findings = QueueDepthRule.check(&events);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].title.contains("23 CBs"));
    }

    #[test]
    fn no_crash_no_finding() {
        let events = vec![ev(
            1_000_000_000,
            Source::MetalHook,
            Payload::MetalCbCommitted {
                cb_id: 1,
                queue_id: 1,
                queue_depth: 99,
                label: None,
            },
        )];
        assert!(QueueDepthRule.check(&events).is_empty());
    }
}

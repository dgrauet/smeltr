//! Advisory rule: flags high queue depth / long in-flight even without a
//! kIOGPU error. Defers to QueueDepthRule when a crash is present.

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

pub struct QueuePressureRule;

const DEPTH_THRESHOLD: u32 = 32;
const IN_FLIGHT_THRESHOLD_NS: u64 = 1_000_000_000;

impl Rule for QueuePressureRule {
    fn name(&self) -> &'static str {
        "queue_pressure"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        let has_crash = events.iter().any(|e| {
            matches!(
                &e.payload,
                Payload::MetalCbCompleted { error_code: Some(c), .. } if *c != 0
            )
        });
        if has_crash {
            return Vec::new();
        }

        let mut max_depth = 0u32;
        let mut max_depth_seq = 0u64;
        let mut max_depth_ts = 0u64;
        let mut max_in_flight_ns = 0u64;
        let mut max_in_flight_cb = 0u64;
        let mut max_in_flight_seq = 0u64;

        for e in events {
            match &e.payload {
                Payload::MetalCbCommitted { queue_depth, .. } if *queue_depth > max_depth => {
                    max_depth = *queue_depth;
                    max_depth_seq = e.seq;
                    max_depth_ts = e.ts_mono_ns;
                }
                Payload::MetalCbCompleted {
                    cb_id,
                    in_flight_ns,
                    ..
                } if *in_flight_ns > max_in_flight_ns => {
                    max_in_flight_ns = *in_flight_ns;
                    max_in_flight_cb = *cb_id;
                    max_in_flight_seq = e.seq;
                }
                _ => {}
            }
        }

        let depth_alarm = max_depth >= DEPTH_THRESHOLD;
        let in_flight_alarm = max_in_flight_ns >= IN_FLIGHT_THRESHOLD_NS;
        if !depth_alarm && !in_flight_alarm {
            return Vec::new();
        }

        let title = format!(
            "Queue pressure: peak depth {max_depth}, max in-flight {}ms",
            max_in_flight_ns / 1_000_000
        );
        let detail = format!(
            "Sustained pressure detected: queue_depth >= {DEPTH_THRESHOLD} or \
             in_flight_ns >= {}s. Consider mx.synchronize() between phases or \
             reducing batch concurrency.",
            IN_FLIGHT_THRESHOLD_NS / 1_000_000_000
        );
        let mut f = Finding::new(Severity::Warning, Category::ContributingFactor, title)
            .with_detail(detail);
        if depth_alarm {
            f = f.with_evidence(EvidenceRef {
                seq: max_depth_seq,
                ts_mono_ns: max_depth_ts,
                description: format!("MetalCbCommitted queue_depth={max_depth}"),
            });
        }
        if in_flight_alarm {
            f = f.with_evidence(EvidenceRef {
                seq: max_in_flight_seq,
                ts_mono_ns: 0,
                description: format!(
                    "MetalCbCompleted cb_id={max_in_flight_cb} in_flight={}ms",
                    max_in_flight_ns / 1_000_000
                ),
            });
        }
        vec![f]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::test_helpers::ev;
    use smeltr_core::event::Source;

    #[test]
    fn fires_on_high_depth_no_crash() {
        let events = vec![ev(
            100,
            Source::MetalHook,
            Payload::MetalCbCommitted {
                cb_id: 1,
                queue_id: 1,
                queue_depth: 40,
                label: None,
            },
        )];
        let f = QueuePressureRule.check(&events);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].category, Category::ContributingFactor);
        assert_eq!(f[0].severity, Severity::Warning);
    }

    #[test]
    fn fires_on_long_in_flight_no_crash() {
        let events = vec![ev(
            100,
            Source::MetalHook,
            Payload::MetalCbCompleted {
                cb_id: 1,
                queue_id: 1,
                status: 4,
                error_code: None,
                error_domain: None,
                in_flight_ns: 1_500_000_000,
            },
        )];
        let f = QueuePressureRule.check(&events);
        assert_eq!(f.len(), 1);
        assert!(f[0].title.contains("1500ms"));
    }

    #[test]
    fn silent_on_modest_workload() {
        let events = vec![ev(
            100,
            Source::MetalHook,
            Payload::MetalCbCommitted {
                cb_id: 1,
                queue_id: 1,
                queue_depth: 5,
                label: None,
            },
        )];
        assert!(QueuePressureRule.check(&events).is_empty());
    }

    #[test]
    fn defers_when_crash_present() {
        let events = vec![
            ev(
                100,
                Source::MetalHook,
                Payload::MetalCbCommitted {
                    cb_id: 1,
                    queue_id: 1,
                    queue_depth: 40,
                    label: None,
                },
            ),
            ev(
                200,
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
        assert!(
            QueuePressureRule.check(&events).is_empty(),
            "should defer to QueueDepthRule on crash"
        );
    }
}

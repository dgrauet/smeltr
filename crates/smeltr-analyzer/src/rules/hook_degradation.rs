//! Surfaces degraded MetalHook capture quality as findings.
//!
//! On the 2026-07-14 LTX-2 session (#113), the hook silently lost op
//! attribution for 60+ minutes; the only trace was one Info-level
//! MetalHookSkipped event drowned among 333k others. Sampling disables,
//! ring corruption and writer drops now get a Warning-level finding so
//! `smeltr analyze` states plainly that op-level numbers are incomplete.

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

pub struct HookDegradationRule;

impl Rule for HookDegradationRule {
    fn name(&self) -> &'static str {
        "hook_degradation"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        let mut out = Vec::new();

        // Sampling disables (stage- or dispatch-boundary): one finding each,
        // they are rare and each marks the start of a degraded span.
        for ev in events {
            if let Payload::MetalHookSkipped { reason } = &ev.payload {
                if reason.contains("sampling disabled") {
                    out.push(
                        Finding::new(
                            Severity::Warning,
                            Category::ContributingFactor,
                            format!("GPU op timing degraded: {reason}"),
                        )
                        .with_detail(
                            "Per-op GPU attribution (origins/op-summary/breakdown) is \
                             incomplete from this point until sampling is re-enabled."
                                .to_string(),
                        )
                        .with_evidence(EvidenceRef {
                            seq: ev.seq,
                            ts_mono_ns: ev.ts_mono_ns,
                            description: format!("MetalHookSkipped: {reason}"),
                        }),
                    );
                }
            }
        }

        // Ring corruption: aggregate into one finding (first occurrence as
        // evidence) — each skipped frame is an unrecoverable lost event.
        let decode_errors: Vec<&Event> = events
            .iter()
            .filter(|ev| {
                matches!(&ev.payload, Payload::MetalHookSkipped { reason }
                    if reason.contains("ring decode error"))
            })
            .collect();
        if let Some(first) = decode_errors.first() {
            let Payload::MetalHookSkipped { reason } = &first.payload else {
                unreachable!()
            };
            out.push(
                Finding::new(
                    Severity::Warning,
                    Category::ContributingFactor,
                    format!(
                        "Metal hook ring corruption: {} corrupt frame report(s) skipped",
                        decode_errors.len()
                    ),
                )
                .with_detail(
                    "Corrupt ring frames were skipped; some Metal events are lost.".to_string(),
                )
                .with_evidence(EvidenceRef {
                    seq: first.seq,
                    ts_mono_ns: first.ts_mono_ns,
                    description: format!("MetalHookSkipped: {reason}"),
                }),
            );
        }

        // Writer drops (ring full): aggregate the total.
        let mut total_dropped: u64 = 0;
        let mut first_drop: Option<&Event> = None;
        for ev in events {
            if let Payload::MetalHookDropped { count } = &ev.payload {
                total_dropped += count;
                first_drop.get_or_insert(ev);
            }
        }
        if let Some(first) = first_drop {
            out.push(
                Finding::new(
                    Severity::Warning,
                    Category::ContributingFactor,
                    format!("Metal hook ring overflow: {total_dropped} event(s) dropped"),
                )
                .with_detail(
                    "The mmap ring filled faster than the daemon drained it; \
                     Metal events were dropped at the source."
                        .to_string(),
                )
                .with_evidence(EvidenceRef {
                    seq: first.seq,
                    ts_mono_ns: first.ts_mono_ns,
                    description: "first MetalHookDropped event".to_string(),
                }),
            );
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
    fn sampling_disable_becomes_warning_finding() {
        let events = vec![ev(
            36_000_000_000,
            Source::MetalHook,
            Payload::MetalHookSkipped {
                reason:
                    "stage sampling disabled after sustained alloc failures (pro-rata fallback)"
                        .to_string(),
            },
        )];
        let findings = HookDegradationRule.check(&events);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].severity, Severity::Warning);
        assert!(findings[0].title.contains("GPU op timing degraded"));
    }

    #[test]
    fn decode_errors_aggregate_into_one_finding() {
        let events = vec![
            ev(
                1,
                Source::MetalHook,
                Payload::MetalHookSkipped {
                    reason: "ring decode error (#1): unknown frame kind 42".to_string(),
                },
            ),
            ev(
                2,
                Source::MetalHook,
                Payload::MetalHookSkipped {
                    reason: "ring decode error (#1000): frame truncated at offset 24".to_string(),
                },
            ),
        ];
        let findings = HookDegradationRule.check(&events);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].title.contains("2 corrupt frame report(s)"));
    }

    #[test]
    fn drops_aggregate_total_count() {
        let events = vec![
            ev(
                1,
                Source::MetalHook,
                Payload::MetalHookDropped { count: 10 },
            ),
            ev(
                2,
                Source::MetalHook,
                Payload::MetalHookDropped { count: 32 },
            ),
        ];
        let findings = HookDegradationRule.check(&events);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].title.contains("42 event(s) dropped"));
    }

    #[test]
    fn other_skips_produce_no_finding() {
        let events = vec![ev(
            1,
            Source::MetalHook,
            Payload::MetalHookSkipped {
                reason: "SMELTR_HOOK_ML_ENCODER=1: no MTL4 ML encoder classes found".to_string(),
            },
        )];
        assert!(HookDegradationRule.check(&events).is_empty());
    }
}

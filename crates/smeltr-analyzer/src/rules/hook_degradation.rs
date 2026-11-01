//! Surfaces degraded MetalHook capture quality as findings.
//!
//! On the 2026-07-14 LTX-2 session (#113), the hook silently lost op
//! attribution for 60+ minutes; the only trace was one Info-level
//! MetalHookSkipped event drowned among 333k others. Sampling disables,
//! ring corruption and writer drops now get a Warning-level finding so
//! `smeltr analyze` states plainly that op-level numbers are incomplete.
//! Per-kernel times split between kernels by dispatch count (#314) are
//! reported here too: they are as untrustworthy, and look exact.

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

        // Sampling disables (stage- and dispatch-boundary separately): the
        // backoff can cycle disable → re-enable many times in one session
        // (a real 15-min run produced 18 cycles), so aggregate each kind
        // into ONE finding with episode counts, first occurrence as evidence.
        for kind in ["stage", "dispatch"] {
            let disable_marker = format!("{kind} sampling disabled");
            let reenable_marker = format!("{kind} sampling re-enabled");
            let mut disables = 0usize;
            let mut reenables = 0usize;
            let mut first: Option<&Event> = None;
            for ev in events {
                if let Payload::MetalHookSkipped { reason } = &ev.payload {
                    if reason.contains(&disable_marker) {
                        disables += 1;
                        first.get_or_insert(ev);
                    } else if reason.contains(&reenable_marker) {
                        reenables += 1;
                    }
                }
            }
            let Some(first) = first else { continue };
            let Payload::MetalHookSkipped { reason } = &first.payload else {
                unreachable!()
            };
            out.push(
                Finding::new(
                    Severity::Warning,
                    Category::ContributingFactor,
                    format!(
                        "GPU op timing degraded: {kind} sampling disabled {disables} time(s) \
                         after sustained alloc failures"
                    ),
                )
                .with_detail(format!(
                    "Per-op GPU attribution (origins/op-summary/breakdown) is incomplete \
                     during the disabled spans (re-enabled {reenables} time(s) by the \
                     backoff retry)."
                ))
                .with_evidence(EvidenceRef {
                    seq: first.seq,
                    ts_mono_ns: first.ts_mono_ns,
                    description: format!("first occurrence — MetalHookSkipped: {reason}"),
                }),
            );
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

        // Per-kernel times split by dispatch count rather than measured
        // (#314): the op tables look exact and are not.
        if let Some(advice) = crate::breakdown::apportioned_advice(events) {
            let first = events.iter().find(|ev| {
                matches!(&ev.payload, Payload::MetalCbOps { ops, .. }
                    if ops.iter().any(|o| o.apportioned_ns.unwrap_or(0) > 0))
            });
            let share = crate::breakdown::ApportionedOps::detect(events)
                .map(|a| a.share() * 100.0)
                .unwrap_or_default();
            let mut finding = Finding::new(
                Severity::Warning,
                Category::ContributingFactor,
                format!(
                    "GPU op times estimated: {share:.0}% split between kernels by dispatch count"
                ),
            )
            .with_detail(advice);
            if let Some(first) = first {
                finding = finding.with_evidence(EvidenceRef {
                    seq: first.seq,
                    ts_mono_ns: first.ts_mono_ns,
                    description: "first MetalCbOps with apportioned op time".to_string(),
                });
            }
            out.push(finding);
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

    fn cb_ops(ts: u64, ops: &[(&str, u64, u64)]) -> Event {
        ev(
            ts,
            Source::MetalHook,
            Payload::MetalCbOps {
                cb_id: 1,
                ops: ops
                    .iter()
                    .map(|(name, gpu_ns, apportioned)| smeltr_core::event::OpSample {
                        name: (*name).into(),
                        symbol: None,
                        gpu_ns: *gpu_ns,
                        count: 1,
                        apportioned_ns: Some(*apportioned),
                    })
                    .collect(),
            },
        )
    }

    /// #314: `smeltr analyze` and `get_session_summary` must say when the
    /// per-kernel split is an estimate, not only the op tables.
    #[test]
    fn apportioned_op_times_become_a_warning() {
        let events = vec![
            cb_ops(10, &[("Gemm", 600, 0)]),
            cb_ops(20, &[("Gemm", 300, 300), ("Add", 100, 100)]),
        ];
        let findings = HookDegradationRule.check(&events);
        assert_eq!(findings.len(), 1, "{findings:#?}");
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warning);
        assert!(f.title.contains("40%"), "{}", f.title);
        assert!(f.detail.contains("--exact-ops"), "{}", f.detail);
        assert_eq!(f.evidence[0].ts_mono_ns, 20, "first apportioned CB");
    }

    #[test]
    fn measured_op_times_raise_nothing() {
        let events = vec![cb_ops(10, &[("Gemm", 600, 0), ("Add", 10, 10)])];
        assert!(HookDegradationRule.check(&events).is_empty());
    }

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

    /// Backoff cycling (disable → re-enable → disable …) must aggregate into
    /// ONE finding with episode counts, not one finding per episode — a real
    /// 15-min run produced 18 disables and drowned the report.
    #[test]
    fn repeated_disable_reenable_cycles_aggregate_into_one_finding() {
        let mut events = Vec::new();
        for i in 0..18u64 {
            events.push(ev(
                i * 2_000_000_000,
                Source::MetalHook,
                Payload::MetalHookSkipped {
                    reason: "stage sampling disabled after sustained alloc failures \
                             (pro-rata fallback; backoff retry scheduled)"
                        .to_string(),
                },
            ));
            events.push(ev(
                i * 2_000_000_000 + 1_000_000_000,
                Source::MetalHook,
                Payload::MetalHookSkipped {
                    reason: "stage sampling re-enabled (backoff retry)".to_string(),
                },
            ));
        }
        let findings = HookDegradationRule.check(&events);
        assert_eq!(findings.len(), 1, "got: {findings:#?}");
        assert!(
            findings[0].title.contains("18"),
            "title: {}",
            findings[0].title
        );
        assert!(
            findings[0].detail.contains("re-enabled 18"),
            "detail: {}",
            findings[0].detail
        );
    }

    /// Stage- and dispatch-boundary disables are distinct degradations and
    /// keep separate findings.
    #[test]
    fn stage_and_dispatch_disables_stay_separate() {
        let events = vec![
            ev(
                1,
                Source::MetalHook,
                Payload::MetalHookSkipped {
                    reason: "stage sampling disabled after sustained alloc failures \
                             (pro-rata fallback; backoff retry scheduled)"
                        .to_string(),
                },
            ),
            ev(
                2,
                Source::MetalHook,
                Payload::MetalHookSkipped {
                    reason: "dispatch sampling disabled after sustained alloc failures \
                             (stage-boundary fallback; backoff retry scheduled)"
                        .to_string(),
                },
            ),
        ];
        let findings = HookDegradationRule.check(&events);
        assert_eq!(findings.len(), 2, "got: {findings:#?}");
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

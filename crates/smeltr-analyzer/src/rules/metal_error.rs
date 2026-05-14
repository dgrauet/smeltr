//! Names Metal GPU command-buffer callback error codes and elevates the
//! known watchdog-class codes to RootCause.

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

pub struct MetalErrorRule;

impl Rule for MetalErrorRule {
    fn name(&self) -> &'static str {
        "metal_error"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        let mut out = Vec::new();
        for ev in events {
            if let Payload::MetalCbCompleted {
                cb_id,
                error_code: Some(c),
                error_domain,
                ..
            } = &ev.payload
            {
                if *c == 0 {
                    continue;
                }
                let name = name_iogpu_error(*c);
                let (severity, category) = if is_watchdog_code(*c) {
                    (Severity::Critical, Category::RootCause)
                } else {
                    (Severity::Warning, Category::ContributingFactor)
                };
                let domain = error_domain.as_deref().unwrap_or("?");
                let title = format!(
                    "Metal command buffer #{cb_id} failed: {name} (code={c}, domain={domain})"
                );
                let evidence = EvidenceRef {
                    seq: ev.seq,
                    ts_mono_ns: ev.ts_mono_ns,
                    description: format!("MetalCbCompleted cb_id={cb_id} error_code={c}"),
                };
                out.push(Finding::new(severity, category, title).with_evidence(evidence));
            }
        }
        out
    }
}

fn name_iogpu_error(code: i64) -> &'static str {
    match code {
        1 => "kIOGPUCommandBufferCallbackErrorTimeout",
        2 => "kIOGPUCommandBufferCallbackErrorOutOfMemory",
        3 => "kIOGPUCommandBufferCallbackErrorPageFault",
        4 => "kIOGPUCommandBufferCallbackErrorAccessViolation",
        5 => "kIOGPUCommandBufferCallbackErrorInvalidResource",
        6 => "kIOGPUCommandBufferCallbackErrorBlacklisted",
        14 => "kIOGPUCommandBufferCallbackErrorImpactingInteractivity",
        _ => "kIOGPUCommandBufferCallbackErrorUnknown",
    }
}

fn is_watchdog_code(code: i64) -> bool {
    matches!(code, 1 | 14)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::test_helpers::ev;
    use smeltr_core::event::Source;

    #[test]
    fn impacting_interactivity_is_root_cause() {
        let events = vec![ev(
            100,
            Source::MetalHook,
            Payload::MetalCbCompleted {
                cb_id: 42,
                queue_id: 1,
                status: 4,
                error_code: Some(14),
                error_domain: Some("IOGPU".into()),
                in_flight_ns: 9_000_000_000,
            },
        )];
        let findings = MetalErrorRule.check(&events);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, Category::RootCause);
        assert!(findings[0].title.contains("ImpactingInteractivity"));
    }

    #[test]
    fn non_watchdog_code_is_contributing() {
        let events = vec![ev(
            100,
            Source::MetalHook,
            Payload::MetalCbCompleted {
                cb_id: 1,
                queue_id: 1,
                status: 4,
                error_code: Some(3),
                error_domain: Some("IOGPU".into()),
                in_flight_ns: 100,
            },
        )];
        let findings = MetalErrorRule.check(&events);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, Category::ContributingFactor);
    }

    #[test]
    fn zero_error_produces_no_finding() {
        let events = vec![ev(
            100,
            Source::MetalHook,
            Payload::MetalCbCompleted {
                cb_id: 1,
                queue_id: 1,
                status: 4,
                error_code: Some(0),
                error_domain: None,
                in_flight_ns: 1,
            },
        )];
        assert!(MetalErrorRule.check(&events).is_empty());
    }
}

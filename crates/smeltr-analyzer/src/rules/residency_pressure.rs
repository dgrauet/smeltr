//! Flags when the resident GPU working set nears or exceeds the recommended budget.

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

const WARN_RATIO: f64 = 0.90;

pub struct ResidencyPressureRule;

impl Rule for ResidencyPressureRule {
    fn name(&self) -> &'static str {
        "residency_pressure"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        // Report only the single worst sample (highest ratio) to avoid one
        // finding per CB boundary.
        let mut worst: Option<(f64, &Event, u64, u64)> = None;
        for ev in events {
            if let Payload::MetalResidencySample {
                resident_bytes,
                recommended_max_bytes,
                ..
            } = &ev.payload
            {
                if *recommended_max_bytes == 0 {
                    continue;
                }
                let ratio = *resident_bytes as f64 / *recommended_max_bytes as f64;
                if ratio < WARN_RATIO {
                    continue;
                }
                if worst.map(|(r, ..)| ratio > r).unwrap_or(true) {
                    worst = Some((ratio, ev, *resident_bytes, *recommended_max_bytes));
                }
            }
        }
        let mut out = Vec::new();
        if let Some((ratio, ev, resident, rec_max)) = worst {
            let (severity, category) = if ratio > 1.0 {
                (Severity::Critical, Category::ContributingFactor)
            } else {
                (Severity::Warning, Category::ContributingFactor)
            };
            let pct = (ratio * 100.0).round() as u64;
            let title = if ratio > 1.0 {
                format!("Resident GPU working set over the recommended budget ({pct}%) — elevated eviction risk")
            } else {
                format!("Resident GPU working set near the recommended budget ({pct}%)")
            };
            out.push(
                Finding::new(severity, category, title)
                    .with_detail(format!(
                        "peak resident {resident} bytes vs recommendedMaxWorkingSetSize {rec_max} bytes"
                    ))
                    .with_evidence(EvidenceRef {
                        seq: ev.seq,
                        ts_mono_ns: ev.ts_mono_ns,
                        description: "MetalResidencySample".to_string(),
                    }),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::Rule;
    use smeltr_core::event::{Event, Payload, Source};
    use uuid::Uuid;

    fn sample(resident: u64, rec_max: u64) -> Event {
        Event {
            ts_mono_ns: 0,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq: 0,
            payload: Payload::MetalResidencySample {
                resident_bytes: resident,
                recommended_max_bytes: rec_max,
                set_count: 1,
                at_event: "cb_committed".into(),
            },
        }
    }

    #[test]
    fn flags_over_budget_critical() {
        let f = ResidencyPressureRule.check(&[sample(1100, 1000)]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Critical);
    }

    #[test]
    fn flags_near_budget_warning() {
        let f = ResidencyPressureRule.check(&[sample(950, 1000)]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Warning);
    }

    #[test]
    fn silent_below_threshold() {
        assert!(ResidencyPressureRule.check(&[sample(500, 1000)]).is_empty());
    }
}

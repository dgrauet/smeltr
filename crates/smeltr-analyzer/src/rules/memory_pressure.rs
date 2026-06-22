//! Flags when allocated GPU memory nears or exceeds the device's recommended
//! working-set budget (`recommendedMaxWorkingSetSize`). Allocated memory is the
//! actionable pressure signal: when it approaches the budget the system starts
//! evicting/paging GPU resources and performance falls off a cliff. Computed
//! from the `MetalDeviceMemSample` events smeltr already records — no extra
//! instrumentation. (MLX requests residency on its whole working set, so
//! allocated tracks what it keeps resident.)

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

const WARN_RATIO: f64 = 0.90;

pub struct MemoryPressureRule;

impl Rule for MemoryPressureRule {
    fn name(&self) -> &'static str {
        "memory_pressure"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        // Track the enclosing Python scope so the finding can name it.
        let mut scope: Vec<String> = Vec::new();
        // Keep only the single worst (highest-ratio) sample to avoid one
        // finding per command-buffer boundary.
        let mut worst: Option<(f64, u64, u64, u64, u64, String)> = None; // ratio, seq, ts, alloc, recmax, scope
        for ev in events {
            match &ev.payload {
                Payload::ModuleEntered { qualname, .. } => scope.push(qualname.clone()),
                Payload::ModuleReturned { .. } => {
                    scope.pop();
                }
                Payload::MetalDeviceMemSample {
                    allocated_bytes,
                    recommended_max_bytes,
                    ..
                } => {
                    if *recommended_max_bytes == 0 {
                        continue;
                    }
                    let ratio = *allocated_bytes as f64 / *recommended_max_bytes as f64;
                    if ratio < WARN_RATIO {
                        continue;
                    }
                    if worst.as_ref().map(|w| ratio > w.0).unwrap_or(true) {
                        let q = scope
                            .last()
                            .cloned()
                            .unwrap_or_else(|| "<unscoped>".to_string());
                        worst = Some((
                            ratio,
                            ev.seq,
                            ev.ts_mono_ns,
                            *allocated_bytes,
                            *recommended_max_bytes,
                            q,
                        ));
                    }
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        if let Some((ratio, seq, ts, alloc, recmax, q)) = worst {
            let severity = if ratio > 1.0 {
                Severity::Critical
            } else {
                Severity::Warning
            };
            let pct = (ratio * 100.0).round() as u64;
            let title = if ratio > 1.0 {
                format!(
                    "GPU memory over the recommended working-set budget ({pct}%) in scope `{q}` — elevated eviction risk"
                )
            } else {
                format!(
                    "GPU memory near the recommended working-set budget ({pct}%) in scope `{q}`"
                )
            };
            out.push(
                Finding::new(severity, Category::ContributingFactor, title)
                    .with_detail(format!(
                        "peak allocated {alloc} bytes vs recommendedMaxWorkingSetSize {recmax} bytes"
                    ))
                    .with_evidence(EvidenceRef {
                        seq,
                        ts_mono_ns: ts,
                        description: "MetalDeviceMemSample".to_string(),
                    }),
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Payload, Source};
    use uuid::Uuid;

    fn ev(seq: u64, payload: Payload) -> Event {
        Event {
            ts_mono_ns: seq,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq,
            payload,
        }
    }

    fn sample(seq: u64, allocated: u64, recmax: u64) -> Event {
        ev(
            seq,
            Payload::MetalDeviceMemSample {
                allocated_bytes: allocated,
                recommended_max_bytes: recmax,
                at_event: "cb_committed".into(),
            },
        )
    }

    fn entered(seq: u64, q: &str) -> Event {
        ev(
            seq,
            Payload::ModuleEntered {
                module_call_id: seq,
                module_def_id: 0,
                qualname: q.into(),
                class_name: String::new(),
                parent_call_id: None,
                depth: 0,
                fields: Default::default(),
            },
        )
    }

    fn returned(seq: u64) -> Event {
        ev(seq, Payload::ModuleReturned { module_call_id: 0 })
    }

    #[test]
    fn silent_below_threshold() {
        assert!(MemoryPressureRule.check(&[sample(0, 500, 1000)]).is_empty());
    }

    #[test]
    fn warning_near_budget() {
        let f = MemoryPressureRule.check(&[sample(0, 950, 1000)]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Warning);
    }

    #[test]
    fn critical_over_budget() {
        let f = MemoryPressureRule.check(&[sample(0, 1100, 1000)]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Critical);
    }

    #[test]
    fn skips_zero_budget() {
        assert!(MemoryPressureRule.check(&[sample(0, 1000, 0)]).is_empty());
    }

    #[test]
    fn attributes_to_enclosing_scope() {
        let evs = vec![entered(0, "vae.decode"), sample(1, 1100, 1000), returned(2)];
        let f = MemoryPressureRule.check(&evs);
        assert_eq!(f.len(), 1);
        assert!(
            f[0].title.contains("vae.decode"),
            "title missing scope: {}",
            f[0].title
        );
    }

    #[test]
    fn reports_only_worst_sample() {
        let evs = vec![
            sample(0, 920, 1000),
            sample(1, 1200, 1000),
            sample(2, 950, 1000),
        ];
        let f = MemoryPressureRule.check(&evs);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Critical); // the 1200/1000 sample wins
    }
}

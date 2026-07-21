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
        // Segment the over-threshold samples into distinct windows (#183):
        // one aggregated percentage hid that VOID's "115%" was two unrelated
        // windows (transition + decode) — a fix to one left the number
        // unchanged and looked like a failure.
        const MERGE_GAP_NS: u64 = 5_000_000_000;
        let windows = crate::memory::over_budget_windows(events, WARN_RATIO, MERGE_GAP_NS);
        let Some(worst) = windows
            .iter()
            .max_by(|a, b| {
                a.peak_ratio()
                    .partial_cmp(&b.peak_ratio())
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .cloned()
        else {
            return Vec::new();
        };

        let t0 = events.first().map(|e| e.ts_mono_ns).unwrap_or(0);
        let rel_s = |ts: u64| ts.saturating_sub(t0) / 1_000_000_000;

        let ratio = worst.peak_ratio();
        let severity = if ratio > 1.0 {
            Severity::Critical
        } else {
            Severity::Warning
        };
        let pct = (ratio * 100.0).round() as u64;
        let q = &worst.peak_scope;
        let title = if ratio > 1.0 {
            format!(
                "GPU memory over the recommended working-set budget ({pct}%) in scope `{q}` — elevated eviction risk"
            )
        } else {
            format!("GPU memory near the recommended working-set budget ({pct}%) in scope `{q}`")
        };

        let mut lines: Vec<String> = windows
            .iter()
            .map(|w| {
                format!(
                    "  t+{}s..t+{}s peak {} bytes ({}%) in scope `{}`",
                    rel_s(w.start_ts_mono_ns),
                    rel_s(w.end_ts_mono_ns),
                    w.peak_bytes,
                    (w.peak_ratio() * 100.0).round() as u64,
                    w.peak_scope,
                )
            })
            .collect();
        const MAX_LINES: usize = 8;
        if lines.len() > MAX_LINES {
            let dropped = lines.len() - MAX_LINES;
            lines.truncate(MAX_LINES);
            lines.push(format!("  ... {dropped} more window(s) elided"));
        }
        let detail = format!(
            "peak allocated {} bytes vs recommendedMaxWorkingSetSize {} bytes; {} over-budget window(s):\n{}",
            worst.peak_bytes,
            worst.recommended_max_bytes,
            windows.len(),
            lines.join("\n"),
        );

        vec![Finding::new(severity, Category::ContributingFactor, title)
            .with_detail(detail)
            .with_evidence(EvidenceRef {
                seq: worst.peak_seq,
                ts_mono_ns: worst.peak_ts_mono_ns,
                description: format!(
                    "MetalDeviceMemSample — worst window's peak at t+{}s",
                    rel_s(worst.peak_ts_mono_ns)
                ),
            })]
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

#[cfg(test)]
mod window_reporting_tests {
    use super::*;
    use smeltr_core::event::{Payload, Source};
    use uuid::Uuid;

    const S: u64 = 1_000_000_000;

    fn ev(ts: u64, payload: Payload) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq: ts,
            payload,
        }
    }

    fn sample(ts: u64, allocated: u64) -> Event {
        ev(
            ts,
            Payload::MetalDeviceMemSample {
                allocated_bytes: allocated,
                recommended_max_bytes: 1000,
                at_event: "cb_committed".into(),
            },
        )
    }

    /// #183 — the VOID confusion: one aggregated % hid that the peak was two
    /// unrelated windows. The detail must list each window with its
    /// session-relative time span and its own peak.
    #[test]
    fn detail_lists_each_window_with_relative_times() {
        let evs = vec![
            ev(
                0,
                Payload::MetalHookSkipped {
                    reason: "t0 anchor".into(),
                },
            ),
            sample(236 * S, 1080),
            sample(241 * S, 1030),
            sample(300 * S, 100),
            sample(488 * S, 1150),
            sample(490 * S, 1100),
        ];
        let f = MemoryPressureRule.check(&evs);
        assert_eq!(f.len(), 1, "{f:#?}");
        assert_eq!(f[0].severity, Severity::Critical);
        let d = &f[0].detail;
        assert!(d.contains("2 over-budget window(s)"), "{d}");
        assert!(d.contains("t+236s..t+241s"), "{d}");
        assert!(d.contains("108%"), "{d}");
        assert!(d.contains("t+488s..t+490s"), "{d}");
        assert!(d.contains("115%"), "{d}");
        // Evidence anchors the WORST window's peak sample.
        assert_eq!(f[0].evidence[0].ts_mono_ns, 488 * S);
        assert!(f[0].evidence[0].description.contains("t+488s"));
    }

    #[test]
    fn single_window_keeps_previous_title_shape() {
        let evs = vec![
            ev(
                0,
                Payload::MetalHookSkipped {
                    reason: "t0".into(),
                },
            ),
            sample(10 * S, 1100),
        ];
        let f = MemoryPressureRule.check(&evs);
        assert_eq!(f.len(), 1);
        assert!(f[0].title.contains("110%"), "{}", f[0].title);
        assert!(
            f[0].detail.contains("1 over-budget window(s)"),
            "{}",
            f[0].detail
        );
        assert!(f[0].detail.contains("t+10s"), "{}", f[0].detail);
    }
}

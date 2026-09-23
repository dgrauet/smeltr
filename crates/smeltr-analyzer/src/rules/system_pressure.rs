//! Flags well-known crash-aggravating processes consuming notable CPU.

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

pub struct SystemPressureRule;

use smeltr_core::event::PRESSURE_PROCESS_NAMES as FLAGGED;

const CPU_THRESHOLD: f32 = 5.0;

impl Rule for SystemPressureRule {
    fn name(&self) -> &'static str {
        "system_pressure"
    }

    /// One finding per flagged process, not per sample: `ProcTop` arrives
    /// every 2 s, so a minute of `spindump` used to yield ~30 identical
    /// warnings (#244). The finding carries the peak and how many samples
    /// crossed the threshold; its evidence is the peak sample.
    fn check(&self, events: &[Event]) -> Vec<Finding> {
        // name -> (samples over threshold, peak cpu, peak seq, peak ts)
        let mut by_name: Vec<(String, u32, f32, u64, u64)> = Vec::new();
        for ev in events {
            if let Payload::ProcTop { top, .. } = &ev.payload {
                for proc in top {
                    if proc.cpu_pct < CPU_THRESHOLD {
                        continue;
                    }
                    if !FLAGGED.iter().any(|n| proc.name.contains(n)) {
                        continue;
                    }
                    match by_name.iter_mut().find(|(n, ..)| *n == proc.name) {
                        Some(slot) => {
                            slot.1 += 1;
                            if proc.cpu_pct > slot.2 {
                                (slot.2, slot.3, slot.4) = (proc.cpu_pct, ev.seq, ev.ts_mono_ns);
                            }
                        }
                        None => by_name.push((
                            proc.name.clone(),
                            1,
                            proc.cpu_pct,
                            ev.seq,
                            ev.ts_mono_ns,
                        )),
                    }
                }
            }
        }
        by_name
            .into_iter()
            .map(|(name, samples, peak, seq, ts)| {
                let title = if samples == 1 {
                    format!("{name} consuming {peak:.1}% CPU")
                } else {
                    format!("{name} consuming up to {peak:.1}% CPU ({samples} samples)")
                };
                Finding::new(Severity::Warning, Category::SystemPressure, title).with_evidence(
                    EvidenceRef {
                        seq,
                        ts_mono_ns: ts,
                        description: format!("peak ProcTop at ts={ts}"),
                    },
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::test_helpers::ev;
    use smeltr_core::event::{ProcEntry, Source};

    #[test]
    fn flags_reportcrash_above_threshold() {
        let events = vec![ev(
            1,
            Source::Proc,
            Payload::ProcTop {
                top: vec![ProcEntry {
                    pid: 100,
                    name: "ReportCrash".into(),
                    cpu_pct: 12.4,
                }],
                flagged: vec![],
            },
        )];
        let findings = SystemPressureRule.check(&events);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].title.contains("ReportCrash"));
        assert!(findings[0].title.contains("12.4%"));
    }

    #[test]
    fn ignores_below_threshold() {
        let events = vec![ev(
            1,
            Source::Proc,
            Payload::ProcTop {
                top: vec![ProcEntry {
                    pid: 100,
                    name: "ReportCrash".into(),
                    cpu_pct: 1.0,
                }],
                flagged: vec![],
            },
        )];
        assert!(SystemPressureRule.check(&events).is_empty());
    }

    #[test]
    fn ignores_unflagged_process() {
        let events = vec![ev(
            1,
            Source::Proc,
            Payload::ProcTop {
                top: vec![ProcEntry {
                    pid: 100,
                    name: "ordinary_app".into(),
                    cpu_pct: 80.0,
                }],
                flagged: vec![],
            },
        )];
        assert!(SystemPressureRule.check(&events).is_empty());
    }

    /// #244: one finding per 2 s sample meant ~30 identical warnings for a
    /// minute of `spindump` — the flood #115 removed from `mlx_timing`.
    #[test]
    fn one_finding_per_process_with_its_peak() {
        let events: Vec<Event> = (1..=30u64)
            .map(|i| {
                ev(
                    i,
                    Source::Proc,
                    Payload::ProcTop {
                        top: vec![ProcEntry {
                            pid: 100,
                            name: "spindump".into(),
                            cpu_pct: if i == 17 { 31.0 } else { 8.0 },
                        }],
                        flagged: vec![],
                    },
                )
            })
            .collect();
        let findings = SystemPressureRule.check(&events);
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert!(findings[0].title.contains("31.0%"), "{}", findings[0].title);
        assert!(
            findings[0].title.contains("30 samples"),
            "{}",
            findings[0].title
        );
        assert_eq!(findings[0].evidence[0].seq, 17, "evidence is the peak");
    }

    /// #245: the proc probe flags UserNotificationCenter (and the README
    /// says so) but this rule kept its own, different list.
    #[test]
    fn flags_every_process_the_probe_flags() {
        for name in smeltr_core::event::PRESSURE_PROCESS_NAMES {
            let events = vec![ev(
                1,
                Source::Proc,
                Payload::ProcTop {
                    top: vec![ProcEntry {
                        pid: 100,
                        name: (*name).into(),
                        cpu_pct: 12.0,
                    }],
                    flagged: vec![],
                },
            )];
            assert_eq!(SystemPressureRule.check(&events).len(), 1, "{name}");
        }
    }
}

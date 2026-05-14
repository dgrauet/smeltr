//! Flags well-known crash-aggravating processes consuming notable CPU.

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

pub struct SystemPressureRule;

const FLAGGED: &[&str] = &[
    "ReportCrash",
    "diagnosticservicesd",
    "crashanalyticsd",
    "spindump",
    "syslogd",
];

const CPU_THRESHOLD: f32 = 5.0;

impl Rule for SystemPressureRule {
    fn name(&self) -> &'static str {
        "system_pressure"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        let mut out = Vec::new();
        for ev in events {
            if let Payload::ProcTop { top, .. } = &ev.payload {
                for proc in top {
                    if proc.cpu_pct < CPU_THRESHOLD {
                        continue;
                    }
                    if !FLAGGED.iter().any(|n| proc.name.contains(n)) {
                        continue;
                    }
                    let title = format!("{} consuming {:.1}% CPU", proc.name, proc.cpu_pct);
                    out.push(
                        Finding::new(Severity::Warning, Category::SystemPressure, title)
                            .with_evidence(EvidenceRef {
                                seq: ev.seq,
                                ts_mono_ns: ev.ts_mono_ns,
                                description: format!("ProcTop at ts={}", ev.ts_mono_ns),
                            }),
                    );
                }
            }
        }
        out
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
}

//! Présomption de mort par jetsam quand le rapport `.ips` manque.
//!
//! Signal plus faible que [`crate::rules::jetsam_kill`] : le rapport peut
//! être écrit après la fin de la session, le répertoire peut être illisible,
//! la génération peut être désactivée. Quand l'empreinte montait franchement
//! et que la session s'arrête net, ça vaut d'être dit — mais comme une
//! présomption, jamais comme un verdict.
//!
//! Aucun seuil jetsam n'est codé ici : la règle regarde une *pente*, pas une
//! limite. La limite par processus n'est pas interrogeable sans droits
//! particuliers sur macOS.

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

/// Croissance minimale entre le premier et le dernier échantillon pour
/// qualifier une pente. Facteur, pas seuil absolu : on ne prétend pas savoir
/// à partir de quelle taille jetsam frappe.
const RISE_FACTOR: f64 = 2.0;

pub struct JetsamRiskRule;

impl Rule for JetsamRiskRule {
    fn name(&self) -> &'static str {
        "jetsam_risk"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        // Un verdict confirmé rend cette présomption inutile.
        if events
            .iter()
            .any(|e| matches!(e.payload, Payload::JetsamKill { .. }))
        {
            return Vec::new();
        }

        let samples: Vec<(&Event, u64)> = events
            .iter()
            .filter_map(|e| match &e.payload {
                Payload::ProcFootprint {
                    phys_footprint_bytes,
                    is_traced_root: true,
                    ..
                } => Some((e, *phys_footprint_bytes)),
                _ => None,
            })
            .collect();

        let (Some((first_ev, first)), Some((last_ev, last))) =
            (samples.first().copied(), samples.last().copied())
        else {
            return Vec::new();
        };
        if first == 0 || (last as f64) < (first as f64) * RISE_FACTOR {
            return Vec::new();
        }

        let gib = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
        vec![Finding::new(
            Severity::Warning,
            Category::ContributingFactor,
            "L'empreinte mémoire montait franchement en fin de session",
        )
        .with_detail(format!(
            "L'empreinte du processus tracé est passée de {:.2} à {:.2} Gio \
             avant que la session s'arrête, et aucun rapport jetsam n'a été \
             joint. Une mort par jetsam est possible : le processus disparaît \
             alors sans traceback ni exception. Vérifiez \
             /Library/Logs/DiagnosticReports pour un JetsamEvent-*.ips \
             postérieur à la fin de la session — il peut être écrit après \
             coup. Ceci est une présomption, pas un verdict.",
            gib(first),
            gib(last)
        ))
        .with_evidence(EvidenceRef {
            seq: first_ev.seq,
            ts_mono_ns: first_ev.ts_mono_ns,
            description: format!("empreinte initiale {:.2} Gio", gib(first)),
        })
        .with_evidence(EvidenceRef {
            seq: last_ev.seq,
            ts_mono_ns: last_ev.ts_mono_ns,
            description: format!("dernière empreinte {:.2} Gio", gib(last)),
        })]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::Severity;
    use crate::rules::test_helpers::ev;
    use smeltr_core::event::{Payload, Source};

    fn footprint(ts: u64, bytes: u64) -> smeltr_core::event::Event {
        ev(
            ts,
            Source::Proc,
            Payload::ProcFootprint {
                pid: 4242,
                name: "python".into(),
                phys_footprint_bytes: bytes,
                lifetime_max_phys_footprint_bytes: bytes,
                is_traced_root: true,
            },
        )
    }

    #[test]
    fn rising_footprint_then_silence_is_a_risk() {
        let events = vec![
            footprint(1, 4_000_000_000),
            footprint(2, 9_000_000_000),
            footprint(3, 18_000_000_000),
        ];
        let f = JetsamRiskRule.check(&events);
        assert_eq!(f.len(), 1, "got: {f:#?}");
        assert_eq!(f[0].severity, Severity::Warning);
        // Vocabulaire de présomption, pas de verdict.
        assert!(
            f[0].detail.contains("peut") || f[0].detail.contains("possible"),
            "detail: {}",
            f[0].detail
        );
    }

    #[test]
    fn flat_footprint_is_not_a_risk() {
        let events = vec![
            footprint(1, 4_000_000_000),
            footprint(2, 4_100_000_000),
            footprint(3, 4_050_000_000),
        ];
        assert!(JetsamRiskRule.check(&events).is_empty());
    }

    #[test]
    fn confirmed_kill_suppresses_the_weaker_signal() {
        // Quand jetsam_kill a le verdict, jetsam_risk se tait : deux findings
        // sur le même fait, c'est du bruit.
        let events = vec![
            footprint(1, 4_000_000_000),
            footprint(2, 18_000_000_000),
            ev(
                3,
                Source::CrashReport,
                Payload::JetsamKill {
                    path: "/x/j.ips".into(),
                    killed_pid: Some(4242),
                    killed_name: "python".into(),
                    footprint_bytes: 21_474_836_480,
                    lifetime_max_bytes: 21_474_836_480,
                    page_size: 16_384,
                    reason: Some("per-process-limit".into()),
                },
            ),
        ];
        assert!(JetsamRiskRule.check(&events).is_empty());
    }

    #[test]
    fn no_footprint_samples_yield_nothing() {
        assert!(JetsamRiskRule.check(&[]).is_empty());
    }
}

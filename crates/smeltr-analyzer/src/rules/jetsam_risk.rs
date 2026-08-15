//! Présomption de mort par jetsam quand le rapport `.ips` manque.
//!
//! Signal plus faible que le verdict rendu par
//! [`crate::crash_join::join_jetsam`] : le rapport peut être écrit après la
//! fin de la session, le répertoire peut être illisible, la génération peut
//! être désactivée. Quand l'empreinte montait franchement et que la session
//! s'arrête net, ça vaut d'être dit — mais comme une présomption, jamais
//! comme un verdict.
//!
//! La pente NE SUFFIT PAS. Le premier échantillon est le tout premier tic de
//! la sonde, à l'attachement, avant que l'enfant ait exec'é sa charge : tout
//! run qui alloue quoi que ce soit franchit le facteur. Il faut aussi une
//! preuve de fin anormale — l'absence de `SessionEnded` dans le flux.
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
        // Un verdict confirmé rend cette présomption inutile ; et une
        // session finalisée proprement n'est pas morte de pression mémoire.
        // `SessionEnded` n'est écrit que par `finalize`, seul chemin vers
        // `ended_rfc3339` — son absence du flux est donc une vraie preuve de
        // fin anormale. (La reprise au boot ne réécrit que les métadonnées,
        // sans ajouter d'événement : elle ne masque pas le signal.)
        if events.iter().any(|e| {
            matches!(
                e.payload,
                Payload::JetsamKill { .. } | Payload::SessionEnded { .. }
            )
        }) {
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

        // Go décimal (1e9), comme `jetsam_finding` : macOS et les rapports
        // jetsam eux-mêmes expriment les empreintes ainsi, et deux findings
        // sur le même fait dans deux unités différentes ne se comparent pas.
        let gb = |b: u64| b as f64 / 1_000_000_000.0;
        vec![Finding::new(
            Severity::Warning,
            Category::ContributingFactor,
            "L'empreinte mémoire montait franchement en fin de session",
        )
        .with_detail(format!(
            "L'empreinte du processus tracé est passée de {:.2} à {:.2} Go \
             avant que la session s'arrête sans fin propre, et aucun rapport \
             jetsam n'a été joint. Une mort par jetsam est possible : le \
             processus disparaît alors sans traceback ni exception. Vérifiez \
             /Library/Logs/DiagnosticReports pour un JetsamEvent-*.ips \
             postérieur à la fin de la session — il peut être écrit après \
             coup. Ceci est une présomption, pas un verdict.",
            gb(first),
            gb(last)
        ))
        .with_evidence(EvidenceRef {
            seq: first_ev.seq,
            ts_mono_ns: first_ev.ts_mono_ns,
            description: format!("empreinte initiale {:.2} Go", gb(first)),
        })
        .with_evidence(EvidenceRef {
            seq: last_ev.seq,
            ts_mono_ns: last_ev.ts_mono_ns,
            description: format!("dernière empreinte {:.2} Go", gb(last)),
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

    fn session_ended(ts: u64) -> smeltr_core::event::Event {
        ev(
            ts,
            Source::System,
            Payload::SessionEnded {
                wall_unix_ns: ts,
                reason: "record:exit pid=4242".into(),
            },
        )
    }

    /// La pente seule fait feu sur pratiquement TOUT run sain : le premier
    /// échantillon est le tout premier tic de la sonde, à l'attachement,
    /// avant même que l'enfant ait exec'é sa charge — quelques Mo — et le
    /// dernier est le pic du run. Un run d'acceptation parfaitement sain est
    /// passé de 15 Mo à 1007 Mo. Un avertissement qui se déclenche toujours
    /// apprend à ignorer toute la liste des findings, y compris le Critical
    /// pour lequel cette branche existe.
    ///
    /// `SessionEnded` n'est écrit que par `finalize` (le seul chemin vers
    /// `ended_rfc3339`) : son absence du FLUX est donc un vrai signal de fin
    /// anormale. La reprise au boot, elle, ne réécrit que les métadonnées et
    /// n'ajoute aucun événement — elle ne masque pas le signal.
    #[test]
    fn a_healthy_rising_session_that_ended_cleanly_is_not_a_risk() {
        let events = vec![
            footprint(1, 15_000_000),
            footprint(2, 500_000_000),
            footprint(3, 1_007_000_000),
            session_ended(4),
        ];
        assert!(
            JetsamRiskRule.check(&events).is_empty(),
            "got: {:#?}",
            JetsamRiskRule.check(&events)
        );
    }

    /// La même pente SANS fin propre reste un risque : c'est la forme que
    /// produit un kill jetsam.
    #[test]
    fn the_same_rise_without_a_clean_end_is_still_a_risk() {
        let events = vec![
            footprint(1, 15_000_000),
            footprint(2, 500_000_000),
            footprint(3, 1_007_000_000),
        ];
        assert_eq!(JetsamRiskRule.check(&events).len(), 1);
    }
}

//! Le cache de l'allocateur MLX retient plus que le travail réel.
//!
//! smeltr mesure par ailleurs au niveau Metal, où un tampon libéré par le
//! programme mais retenu par MLX est indistinct de la mémoire de travail.
//! `mx.get_cache_memory()` fait la différence, et c'est celui qui compte :
//! un composant qui élargit une limite de cache posée en amont fait gonfler
//! le cache sans que rien ne le signale (ltx-2-mlx#79 — 8 à 21 Go pendant le
//! denoise).
//!
//! La règle compare deux **pics**, pas un ratio instantané. Entre deux
//! `mx.eval` l'actif frôle zéro alors que le cache reste plein : mesuré sur
//! le store réel, le rapport cache/actif à un instant donné monte jusqu'à
//! plusieurs centaines de millions, ce qui n'apprend rien. Comparer le pic
//! de cache au pic d'actif répond en revanche à la bonne question — MLX
//! retient-il plus que ce que le programme a jamais utilisé ?

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

/// Rapport pic-cache / pic-actif à partir duquel on signale.
///
/// Calibré sur les 83 sessions du store local portant des `MlxMemoryPoll` :
/// 13 dépassent 1.0, dont le cluster ltx-2-mlx#79 entre 1.40 et 1.49. À 1.25
/// la règle retient 9 sessions (11 %) — le cluster #79 garde une marge
/// confortable, et les cas marginaux à 1.09 et 1.12 sont écartés. Les
/// sessions silencieuses incluent des caches PLUS gros (27,6 Go) avec un
/// actif plus grand encore : du cache proportionnel au travail, légitime.
const CACHE_TO_ACTIVE_RATIO: f64 = 1.25;

/// Plancher absolu, pour ne rien dire des petits runs où le rapport n'a pas
/// de sens. Ce n'est pas un seuil de gravité : c'est un anti-bruit.
const MIN_CACHE_BYTES: u64 = 1_000_000_000;

pub struct MlxCacheGrowthRule;

impl Rule for MlxCacheGrowthRule {
    fn name(&self) -> &'static str {
        "mlx_cache_growth"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        let mut peak_cache = 0u64;
        let mut peak_active = 0u64;
        let mut at_peak: Option<&Event> = None;
        for e in events {
            if let Payload::MlxMemoryPoll {
                active_bytes,
                cache_bytes,
                ..
            } = &e.payload
            {
                if *cache_bytes > peak_cache {
                    peak_cache = *cache_bytes;
                    at_peak = Some(e);
                }
                peak_active = peak_active.max(*active_bytes);
            }
        }

        let Some(at_peak) = at_peak else {
            return Vec::new();
        };
        if peak_cache < MIN_CACHE_BYTES || peak_active == 0 {
            return Vec::new();
        }
        let ratio = peak_cache as f64 / peak_active as f64;
        if ratio < CACHE_TO_ACTIVE_RATIO {
            return Vec::new();
        }

        let gb = |b: u64| b as f64 / 1_000_000_000.0;
        vec![Finding::new(
            Severity::Warning,
            Category::ContributingFactor,
            "Le cache de l'allocateur MLX dépasse la mémoire réellement utilisée",
        )
        .with_detail(format!(
            "Le cache MLX a atteint {:.2} Go alors que la mémoire active n'a \
             jamais dépassé {:.2} Go (×{:.2}). Ce sont des tampons libérés par \
             le programme et retenus par MLX : depuis Metal ils sont \
             indistincts de la mémoire de travail, ce qui fait passer le \
             problème pour un besoin réel. Vérifiez qu'aucun composant \
             n'élargit une limite de cache posée en amont — \
             `mx.set_cache_limit()` — et, si la mémoire est contrainte, \
             envisagez `mx.clear_cache()` entre les étapes.",
            gb(peak_cache),
            gb(peak_active),
            ratio
        ))
        .with_evidence(EvidenceRef {
            seq: at_peak.seq,
            ts_mono_ns: at_peak.ts_mono_ns,
            description: format!("pic de cache {:.2} Go", gb(peak_cache)),
        })]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::test_helpers::ev;
    use smeltr_core::event::Source;

    fn poll(ts: u64, active: u64, cache: u64) -> Event {
        ev(
            ts,
            Source::PythonSidecar,
            Payload::MlxMemoryPoll {
                active_bytes: active,
                peak_bytes: active,
                cache_bytes: cache,
            },
        )
    }

    /// La signature de ltx-2-mlx#79, telle qu'observée dans le store :
    /// 21,31 Go de cache pour 15,25 Go d'actif.
    #[test]
    fn cache_above_active_is_reported() {
        let events = vec![
            poll(1, 8_000_000_000, 8_000_000_000),
            poll(2, 15_250_000_000, 21_310_000_000),
        ];
        let f = MlxCacheGrowthRule.check(&events);
        assert_eq!(f.len(), 1, "{f:#?}");
        assert_eq!(f[0].severity, Severity::Warning);
        assert!(f[0].detail.contains("21.31"), "detail: {}", f[0].detail);
        assert!(f[0].detail.contains("15.25"), "detail: {}", f[0].detail);
    }

    /// Du cache proportionnel au travail est légitime, même très gros — le
    /// cas des sessions silencieuses du store, jusqu'à 27,6 Go de cache pour
    /// 49,5 Go d'actif.
    #[test]
    fn cache_below_active_is_not_reported() {
        let events = vec![
            poll(1, 20_000_000_000, 10_000_000_000),
            poll(2, 49_480_000_000, 27_580_000_000),
        ];
        assert!(MlxCacheGrowthRule.check(&events).is_empty());
    }

    /// Juste sous le seuil : on se tait. Épingle le seuil calibré.
    #[test]
    fn a_ratio_below_the_threshold_is_not_reported() {
        let events = vec![poll(1, 8_530_000_000, 9_260_000_000)]; // ×1.09
        assert!(MlxCacheGrowthRule.check(&events).is_empty());
    }

    /// Le plancher anti-bruit : un petit run au rapport élevé ne dit rien.
    #[test]
    fn a_small_cache_is_not_reported() {
        let events = vec![poll(1, 10_000_000, 100_000_000)]; // ×10 mais 100 Mo
        assert!(MlxCacheGrowthRule.check(&events).is_empty());
    }

    /// Sans échantillon MLX — session sans sidecar — la règle se tait.
    #[test]
    fn no_samples_yields_nothing() {
        assert!(MlxCacheGrowthRule.check(&[]).is_empty());
    }

    /// Un actif resté nul ne permet aucun rapport : on ne divise pas.
    #[test]
    fn a_zero_active_yields_nothing() {
        let events = vec![poll(1, 0, 5_000_000_000)];
        assert!(MlxCacheGrowthRule.check(&events).is_empty());
    }
}

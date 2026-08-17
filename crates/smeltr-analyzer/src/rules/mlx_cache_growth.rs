//! The MLX allocator cache retains more than the real working set.
//!
//! smeltr also measures at the Metal level, where a buffer the program freed
//! but MLX kept is indistinguishable from working memory.
//! `mx.get_cache_memory()` tells them apart, and that is the one that matters:
//! a component widening a cache limit set upstream inflates the cache with
//! nothing to flag it (ltx-2-mlx#79 — 8 to 21 GB during the denoise).
//!
//! The rule compares two **peaks**, not an instantaneous ratio. Between two
//! `mx.eval` calls the active figure grazes zero while the cache stays full:
//! measured against the real store, the cache-to-active ratio at a given
//! instant climbs into the hundreds of millions, which teaches nothing.
//! Comparing peak cache against peak active does answer the right question —
//! is MLX retaining more than the program ever used?

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

/// Peak-cache over peak-active ratio at which we raise a finding.
///
/// Calibrated against the 83 sessions in the local store carrying
/// `MlxMemoryPoll` samples: 13 exceed 1.0, including the ltx-2-mlx#79 cluster
/// between 1.40 and 1.49. At 1.25 the rule keeps 9 sessions (11%) — the #79
/// cluster retains a comfortable margin, and the marginal cases at 1.09 and
/// 1.12 are excluded. The silent sessions include BIGGER caches (27.6 GB) with
/// a larger active figure still: cache proportional to the work, legitimately.
const CACHE_TO_ACTIVE_RATIO: f64 = 1.25;

/// Absolute floor, so we say nothing about small runs where the ratio is
/// meaningless. This is not a severity threshold: it is noise suppression.
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

        let gb = smeltr_core::fmt::decimal_gb;
        vec![Finding::new(
            Severity::Warning,
            Category::ContributingFactor,
            "The MLX allocator cache exceeds the memory actually in use",
        )
        .with_detail(format!(
            "The MLX cache reached {:.2} GB while active memory never went \
             above {:.2} GB (x{:.2}). These are buffers the program freed and \
             MLX kept: seen from Metal they are indistinguishable from working \
             memory, which makes the problem look like a genuine requirement. \
             Check that no component widens a cache limit set upstream — \
             `mx.set_cache_limit()` — and, if memory is tight, consider \
             `mx.clear_cache()` between stages.",
            gb(peak_cache),
            gb(peak_active),
            ratio
        ))
        .with_evidence(EvidenceRef {
            seq: at_peak.seq,
            ts_mono_ns: at_peak.ts_mono_ns,
            description: format!("cache peak {:.2} GB", gb(peak_cache)),
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

    /// The ltx-2-mlx#79 signature, as observed in the store: 21.31 GB of cache
    /// against 15.25 GB of active memory.
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

    /// Cache proportional to the work is legitimate, even when very large —
    /// the case of the store's silent sessions, up to 27.6 GB of cache against
    /// 49.5 GB of active memory.
    #[test]
    fn cache_below_active_is_not_reported() {
        let events = vec![
            poll(1, 20_000_000_000, 10_000_000_000),
            poll(2, 49_480_000_000, 27_580_000_000),
        ];
        assert!(MlxCacheGrowthRule.check(&events).is_empty());
    }

    /// Just below the threshold: we stay silent. Pins the calibrated value.
    #[test]
    fn a_ratio_below_the_threshold_is_not_reported() {
        let events = vec![poll(1, 8_530_000_000, 9_260_000_000)]; // x1.09
        assert!(MlxCacheGrowthRule.check(&events).is_empty());
    }

    /// The noise floor: a small run with a high ratio says nothing.
    #[test]
    fn a_small_cache_is_not_reported() {
        let events = vec![poll(1, 10_000_000, 100_000_000)]; // x10 but 100 MB
        assert!(MlxCacheGrowthRule.check(&events).is_empty());
    }

    /// With no MLX samples — a session without the sidecar — the rule is quiet.
    #[test]
    fn no_samples_yields_nothing() {
        assert!(MlxCacheGrowthRule.check(&[]).is_empty());
    }

    /// An active figure that stayed at zero allows no ratio: we do not divide.
    #[test]
    fn a_zero_active_yields_nothing() {
        let events = vec![poll(1, 0, 5_000_000_000)];
        assert!(MlxCacheGrowthRule.check(&events).is_empty());
    }
}

//! Flags model files loaded more than once without an intervening ModelUnload.
//!
//! A duplicate is defined as a `ModelLoad` event where the same canonical path
//! is already present in the "currently loaded" set (i.e. no `ModelUnload` for
//! that path has been seen since the previous load). Loading → unloading →
//! reloading is normal and is NOT flagged.

use std::collections::HashMap;

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

pub struct DuplicateModelLoadRule;

impl Rule for DuplicateModelLoadRule {
    fn name(&self) -> &'static str {
        "duplicate-model-load"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        // Walk events in chronological order (by seq, which is monotonic).
        // currently_loaded: canonical_path → (t_start_ns, size_bytes, seq) of the last load.
        let mut currently_loaded: HashMap<String, (u64, u64, u64)> = HashMap::new();
        let mut out = Vec::new();

        for ev in events {
            match &ev.payload {
                Payload::ModelLoad {
                    path,
                    size_bytes,
                    t_start_ns,
                    ..
                } => {
                    if let Some((first_t_start, _, _)) = currently_loaded.get(path.as_str()) {
                        // Already loaded — this is a duplicate.
                        let mb = *size_bytes as f64 / 1_048_576.0;
                        let basename = std::path::Path::new(path)
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or(path.as_str());
                        let title = format!(
                            "Model {basename} loaded again without prior unload \
                             (size={mb:.1} MB)"
                        );
                        let detail = format!(
                            "Duplicate load of {path}: first at t={first_t_start} ns, \
                             duplicate at t={t_start_ns} ns, size={size_bytes} bytes"
                        );
                        let evidence = EvidenceRef {
                            seq: ev.seq,
                            ts_mono_ns: *t_start_ns,
                            description: format!("ModelLoad path={path} size_bytes={size_bytes}"),
                        };
                        out.push(
                            Finding::new(Severity::Warning, Category::ContributingFactor, &title)
                                .with_detail(detail)
                                .with_evidence(evidence),
                        );
                        // Update to the new load's info.
                        currently_loaded.insert(path.clone(), (*t_start_ns, *size_bytes, ev.seq));
                    } else {
                        currently_loaded.insert(path.clone(), (*t_start_ns, *size_bytes, ev.seq));
                    }
                }
                Payload::ModelUnload { path, .. } => {
                    currently_loaded.remove(path.as_str());
                }
                _ => {}
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::test_helpers::ev;
    use smeltr_core::event::Source;

    fn model_load_event(ts: u64, path: &str, size_bytes: u64) -> Event {
        ev(
            ts,
            Source::PythonSidecar,
            Payload::ModelLoad {
                path: path.into(),
                size_bytes,
                t_start_ns: ts,
                t_end_ns: ts + 500_000_000,
                sha8: None,
                framework: Some("safetensors".into()),
            },
        )
    }

    fn model_unload_event(ts: u64, path: &str) -> Event {
        ev(
            ts,
            Source::PythonSidecar,
            Payload::ModelUnload {
                path: path.into(),
                t_ns: ts,
                sha8: None,
            },
        )
    }

    #[test]
    fn single_load_produces_no_finding() {
        let events = vec![model_load_event(
            1_000_000_000,
            "/models/gemma/model.safetensors",
            2_000_000_000,
        )];
        let findings = DuplicateModelLoadRule.check(&events);
        assert!(
            findings.is_empty(),
            "single load must not produce a finding"
        );
    }

    #[test]
    fn two_loads_same_path_without_unload_produce_one_finding() {
        let events = vec![
            model_load_event(
                1_000_000_000,
                "/models/gemma/model.safetensors",
                2_000_000_000,
            ),
            model_load_event(
                5_000_000_000,
                "/models/gemma/model.safetensors",
                2_000_000_000,
            ),
        ];
        let findings = DuplicateModelLoadRule.check(&events);
        assert_eq!(findings.len(), 1, "two identical loads → one finding");
        assert_eq!(findings[0].severity, Severity::Warning);
        assert_eq!(findings[0].category, Category::ContributingFactor);
        assert!(
            findings[0]
                .detail
                .contains("/models/gemma/model.safetensors"),
            "detail must include canonical path"
        );
    }

    #[test]
    fn two_loads_same_basename_different_paths_produce_no_finding() {
        let events = vec![
            model_load_event(
                1_000_000_000,
                "/models/gemma-2b/model.safetensors",
                2_000_000_000,
            ),
            model_load_event(
                2_000_000_000,
                "/models/gemma-7b/model.safetensors",
                2_000_000_000,
            ),
        ];
        // Same basename + size_bytes, but different paths → not a duplicate.
        let findings = DuplicateModelLoadRule.check(&events);
        assert!(
            findings.is_empty(),
            "different paths must not be flagged even with the same basename"
        );
    }

    #[test]
    fn three_loads_same_path_without_unload_produce_two_findings() {
        let events = vec![
            model_load_event(
                1_000_000_000,
                "/models/gemma/model.safetensors",
                2_000_000_000,
            ),
            model_load_event(
                5_000_000_000,
                "/models/gemma/model.safetensors",
                2_000_000_000,
            ),
            model_load_event(
                9_000_000_000,
                "/models/gemma/model.safetensors",
                2_000_000_000,
            ),
        ];
        let findings = DuplicateModelLoadRule.check(&events);
        assert_eq!(
            findings.len(),
            2,
            "three loads of the same path (no unload) → two duplicate findings"
        );
        // All findings should reference the same path
        for f in &findings {
            assert!(f.detail.contains("/models/gemma/model.safetensors"));
        }
    }

    #[test]
    fn load_then_unload_then_reload_produces_no_finding() {
        // load → unload → reload is a legitimate pattern — must NOT flag.
        let events = vec![
            model_load_event(
                1_000_000_000,
                "/models/gemma/model.safetensors",
                2_000_000_000,
            ),
            model_unload_event(4_000_000_000, "/models/gemma/model.safetensors"),
            model_load_event(
                6_000_000_000,
                "/models/gemma/model.safetensors",
                2_000_000_000,
            ),
        ];
        let findings = DuplicateModelLoadRule.check(&events);
        assert!(
            findings.is_empty(),
            "load → unload → reload must produce no finding, got {findings:?}"
        );
    }

    #[test]
    fn load_load_unload_produces_one_finding() {
        // load → load (duplicate) → unload: the second load fires before the unload → 1 finding.
        let events = vec![
            model_load_event(
                1_000_000_000,
                "/models/gemma/model.safetensors",
                2_000_000_000,
            ),
            model_load_event(
                5_000_000_000,
                "/models/gemma/model.safetensors",
                2_000_000_000,
            ),
            model_unload_event(9_000_000_000, "/models/gemma/model.safetensors"),
        ];
        let findings = DuplicateModelLoadRule.check(&events);
        assert_eq!(
            findings.len(),
            1,
            "load → load → unload → 1 finding (second load precedes the unload)"
        );
    }
}

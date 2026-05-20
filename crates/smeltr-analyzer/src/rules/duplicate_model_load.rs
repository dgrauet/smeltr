//! Flags model files loaded more than once in a single session.
//!
//! A duplicate is defined as two or more `ModelLoad` events with the same
//! *exact* canonical path and `size_bytes`. Different paths that share the
//! same basename are NOT considered duplicates — they may be different copies
//! of the same model file.

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
        // key: (canonical_path, size_bytes) → all (t_start_ns, seq) occurrences
        let mut seen: HashMap<(String, u64), Vec<(u64, u64)>> = HashMap::new();

        for ev in events {
            if let Payload::ModelLoad {
                path,
                size_bytes,
                t_start_ns,
                ..
            } = &ev.payload
            {
                seen.entry((path.clone(), *size_bytes))
                    .or_default()
                    .push((*t_start_ns, ev.seq));
            }
        }

        let mut out = Vec::new();

        for ((path, size_bytes), occurrences) in &seen {
            if occurrences.len() < 2 {
                continue;
            }

            let n = occurrences.len();
            let mb = *size_bytes as f64 / 1_048_576.0;
            let basename = std::path::Path::new(path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(path.as_str());

            let title = format!("Model {basename} loaded {n} times (size={mb:.1} MB)",);

            // First occurrence is the "original"; all subsequent are duplicates.
            let (first_t_start, _) = occurrences[0];

            for (dup_t_start, dup_seq) in occurrences.iter().skip(1) {
                let detail = format!(
                    "Duplicate load of {path}: first at t={first_t_start} ns, \
                     duplicate at t={dup_t_start} ns, size={size_bytes} bytes"
                );
                let evidence = EvidenceRef {
                    seq: *dup_seq,
                    ts_mono_ns: *dup_t_start,
                    description: format!("ModelLoad path={path} size_bytes={size_bytes}"),
                };
                out.push(
                    Finding::new(Severity::Warning, Category::ContributingFactor, &title)
                        .with_detail(detail)
                        .with_evidence(evidence),
                );
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
    fn two_loads_same_path_and_size_produce_one_finding() {
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
            findings[0].title.contains("2 times"),
            "title must mention N=2"
        );
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
    fn three_loads_same_path_produce_two_findings() {
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
            "three loads of the same path → two duplicate findings"
        );
        // All findings should reference the same path
        for f in &findings {
            assert!(f.detail.contains("/models/gemma/model.safetensors"));
        }
    }
}

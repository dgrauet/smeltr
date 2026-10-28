//! Flags weights files whose loaded tensors were never read (#308).
//!
//! The Python sidecar tracks which keys of the dict returned by `mx.load` /
//! `safetensors.torch.load_file` had their value read, and reports the rest
//! on `ModelUnload`. A loader that silently drops part of a file — ltx-2-mlx
//! #235 iterated every key of a LoRA and skipped the ones outside the
//! transformer blocks — leaves no other trace: no error, and no timing or
//! memory anomaly to tell apart from a correct run.
//!
//! Keys are grouped by pattern, numeric path segments collapsed to `*`, so a
//! whole missing family of layers reads as one line.

use std::collections::HashMap;

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

/// Patterns listed in a finding's detail; the rest are summed into one line.
const MAX_PATTERNS: usize = 12;

pub struct UnreadModelTensorsRule;

impl Rule for UnreadModelTensorsRule {
    fn name(&self) -> &'static str {
        "unread-model-tensors"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        let mut out = Vec::new();
        for ev in events {
            let Payload::ModelUnload {
                path,
                tensor_count,
                unread_count: Some(unread),
                unread_keys,
                requested_path,
                ..
            } = &ev.payload
            else {
                continue;
            };
            if *unread == 0 {
                continue;
            }
            let basename = smeltr_core::fmt::basename(requested_path.as_deref().unwrap_or(path));
            let shown_path = match requested_path {
                Some(requested) => format!("{requested} ({path})"),
                None => path.clone(),
            };
            let of_total = tensor_count
                .map(|total| format!(" of {total}"))
                .unwrap_or_default();
            let title = format!("{basename}: {unread}{of_total} loaded tensor(s) were never read");
            out.push(
                Finding::new(Severity::Warning, Category::ContributingFactor, title)
                    .with_detail(detail(&shown_path, *unread, unread_keys))
                    .with_evidence(EvidenceRef {
                        seq: ev.seq,
                        ts_mono_ns: ev.ts_mono_ns,
                        description: format!("ModelUnload path={path} unread_count={unread}"),
                    }),
            );
        }
        out
    }
}

fn detail(path: &str, unread: u64, keys: &[String]) -> String {
    let mut text = format!(
        "The program loaded {path} but never read the value of {unread} of its tensors \
         before releasing it: the loader skipped them. A key-mapping filter that drops \
         part of a file (a prefix check, a renamed module) leaves no other trace — the \
         model runs, only partly patched. Tensors that are legitimately unused (another \
         component's weights in a shared file) also show up here.\n\nUnread keys by \
         pattern:"
    );
    let patterns = group_by_pattern(keys);
    for (pattern, count) in patterns.iter().take(MAX_PATTERNS) {
        text.push_str(&format!("\n  {pattern}  ×{count}"));
    }
    if patterns.len() > MAX_PATTERNS {
        let rest: usize = patterns[MAX_PATTERNS..].iter().map(|(_, n)| n).sum();
        let more = patterns.len() - MAX_PATTERNS;
        text.push_str(&format!("\n  … {more} more pattern(s), {rest} key(s)"));
    }
    if (keys.len() as u64) < unread {
        text.push_str(&format!(
            "\n(patterns cover the first {} unread keys the sidecar reported)",
            keys.len()
        ));
    }
    text
}

/// Counts keys per pattern, most frequent first, then alphabetically.
///
/// When the full patterns do not fit in `MAX_PATTERNS` lines, they are cut to
/// the deepest prefix that does (`a.b.c.lora_A.weight` → `a.b.*`): the keys
/// ltx-2-mlx #235 dropped have no numeric segment to collapse, and 56 of them
/// read as 12 modules. If no depth fits, the full patterns are kept and the
/// caller sums the overflow.
fn group_by_pattern(keys: &[String]) -> Vec<(String, usize)> {
    let patterns: Vec<String> = keys.iter().map(|k| pattern_of(k)).collect();
    let max_depth = patterns
        .iter()
        .map(|p| p.split('.').count())
        .max()
        .unwrap_or(0);
    let full = count_at_depth(&patterns, max_depth);
    if full.len() <= MAX_PATTERNS {
        return full;
    }
    (1..max_depth)
        .rev()
        .map(|depth| count_at_depth(&patterns, depth))
        .find(|groups| groups.len() <= MAX_PATTERNS)
        .unwrap_or(full)
}

fn count_at_depth(patterns: &[String], depth: usize) -> Vec<(String, usize)> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for pattern in patterns {
        let segments: Vec<&str> = pattern.split('.').collect();
        let key = if segments.len() > depth {
            format!("{}.*", segments[..depth].join("."))
        } else {
            pattern.clone()
        };
        *counts.entry(key).or_default() += 1;
    }
    let mut out: Vec<(String, usize)> = counts.into_iter().collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out
}

/// `blocks.12.attn.to_q.lora_A.weight` → `blocks.*.attn.to_q.lora_A.weight`.
fn pattern_of(key: &str) -> String {
    key.split('.')
        .map(|seg| {
            if !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()) {
                "*"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::test_helpers::ev;
    use smeltr_core::event::Source;

    fn unload(tensor_count: Option<u64>, unread_count: Option<u64>, keys: &[&str]) -> Event {
        ev(
            1_000,
            Source::PythonSidecar,
            Payload::ModelUnload {
                path: "/loras/distilled-lora.safetensors".into(),
                t_ns: 1_000,
                sha8: None,
                tensor_count,
                unread_count,
                unread_keys: keys.iter().map(|k| k.to_string()).collect(),
                requested_path: None,
            },
        )
    }

    #[test]
    fn unread_tensors_produce_a_warning_naming_the_file_and_counts() {
        let events = vec![unload(
            Some(1344),
            Some(4),
            &[
                "proj_in.lora_A.weight",
                "proj_in.lora_B.weight",
                "proj_out.lora_A.weight",
                "proj_out.lora_B.weight",
            ],
        )];
        let findings = UnreadModelTensorsRule.check(&events);
        assert_eq!(findings.len(), 1);
        let f = &findings[0];
        assert_eq!(f.severity, Severity::Warning);
        assert_eq!(
            f.title,
            "distilled-lora.safetensors: 4 of 1344 loaded tensor(s) were never read"
        );
        assert!(
            f.detail.contains("proj_in.lora_A.weight  ×1"),
            "{}",
            f.detail
        );
        assert!(f.detail.contains("proj_out.lora_B.weight  ×1"));
    }

    #[test]
    fn title_uses_the_path_the_program_passed_over_the_cache_blob() {
        let mut ev = unload(Some(3320), Some(56), &["proj_in.lora_A.weight"]);
        if let Payload::ModelUnload {
            path,
            requested_path,
            ..
        } = &mut ev.payload
        {
            *path = "/hf/blobs/2943ab994f3c9d88".into();
            *requested_path = Some("/hf/snapshots/abc/distilled-lora-384.safetensors".into());
        }
        let findings = UnreadModelTensorsRule.check(&[ev]);
        assert_eq!(
            findings[0].title,
            "distilled-lora-384.safetensors: 56 of 3320 loaded tensor(s) were never read"
        );
        assert!(findings[0].detail.contains(
            "/hf/snapshots/abc/distilled-lora-384.safetensors (/hf/blobs/2943ab994f3c9d88)"
        ));
    }

    #[test]
    fn numbered_layers_collapse_into_one_pattern() {
        let keys: Vec<String> = (0..48)
            .flat_map(|i| {
                [
                    format!("blocks.{i}.attn1.to_q.lora_A.weight"),
                    format!("blocks.{i}.attn1.to_q.lora_B.weight"),
                ]
            })
            .collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let findings = UnreadModelTensorsRule.check(&[unload(Some(96), Some(96), &refs)]);
        let detail = &findings[0].detail;
        assert!(
            detail.contains("blocks.*.attn1.to_q.lora_A.weight  ×48"),
            "{detail}"
        );
        assert!(detail.contains("blocks.*.attn1.to_q.lora_B.weight  ×48"));
        assert!(!detail.contains("blocks.0."));
    }

    /// The 56 keys ltx-2-mlx #235 dropped (distilled LoRA, outside the blocks):
    /// 8 AdaLN modules × 6 factors and 4 projections × 2 — no numeric segment.
    fn ltx_235_keys() -> Vec<String> {
        let mut keys = Vec::new();
        for adaln in [
            "adaln_single",
            "audio_adaln_single",
            "prompt_adaln_single",
            "audio_prompt_adaln_single",
            "av_ca_a2v_gate_adaln_single",
            "av_ca_v2a_gate_adaln_single",
            "av_ca_video_scale_shift_adaln_single",
            "av_ca_audio_scale_shift_adaln_single",
        ] {
            for lin in [
                "emb.timestep_embedder.linear_1",
                "emb.timestep_embedder.linear_2",
                "linear",
            ] {
                for f in ["lora_A", "lora_B"] {
                    keys.push(format!("diffusion_model.{adaln}.{lin}.{f}.weight"));
                }
            }
        }
        for proj in ["proj_in", "proj_out", "audio_proj_in", "audio_proj_out"] {
            for f in ["lora_A", "lora_B"] {
                keys.push(format!("diffusion_model.{proj}.{f}.weight"));
            }
        }
        keys
    }

    #[test]
    fn too_many_patterns_are_shortened_to_the_deepest_prefix_that_fits() {
        let keys = ltx_235_keys();
        assert_eq!(keys.len(), 56);
        let groups = group_by_pattern(&keys);
        assert_eq!(groups.len(), 12, "{groups:?}");
        assert!(groups.contains(&("diffusion_model.adaln_single.*".into(), 6)));
        assert!(groups.contains(&("diffusion_model.proj_out.*".into(), 2)));
        assert_eq!(groups.iter().map(|(_, n)| n).sum::<usize>(), 56);
    }

    #[test]
    fn patterns_that_fit_keep_their_full_depth() {
        let keys: Vec<String> = ["a.b.lora_A.weight", "a.b.lora_B.weight"]
            .iter()
            .map(|k| k.to_string())
            .collect();
        let groups = group_by_pattern(&keys);
        assert_eq!(
            groups,
            vec![
                ("a.b.lora_A.weight".into(), 1),
                ("a.b.lora_B.weight".into(), 1)
            ]
        );
    }

    #[test]
    fn digits_inside_a_segment_are_kept() {
        assert_eq!(
            pattern_of("down_blocks.1.conv2d.weight"),
            "down_blocks.*.conv2d.weight"
        );
        assert_eq!(pattern_of("layer3"), "layer3");
    }

    #[test]
    fn fully_read_or_untracked_unloads_are_silent() {
        let events = vec![unload(Some(10), Some(0), &[]), unload(None, None, &[])];
        assert!(UnreadModelTensorsRule.check(&events).is_empty());
    }

    #[test]
    fn capped_key_list_is_disclosed() {
        let findings =
            UnreadModelTensorsRule.check(&[unload(Some(1000), Some(1000), &["a.weight"])]);
        assert!(
            findings[0]
                .detail
                .contains("patterns cover the first 1 unread keys"),
            "{}",
            findings[0].detail
        );
    }

    #[test]
    fn patterns_beyond_the_limit_are_summed() {
        let keys: Vec<String> = (0..20).map(|i| format!("module_{i}.weight")).collect();
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let findings = UnreadModelTensorsRule.check(&[unload(Some(20), Some(20), &refs)]);
        assert!(
            findings[0].detail.contains("… 8 more pattern(s), 8 key(s)"),
            "{}",
            findings[0].detail
        );
    }

    #[test]
    fn registered_in_all_rules() {
        assert!(crate::rule::all_rules()
            .iter()
            .any(|r| r.name() == "unread-model-tensors"));
    }
}

//! Surfaces "Metal hook active but Python sidecar absent" (#178).
//!
//! A fresh install typically has the hook working on the first `smeltr
//! record` but no `smeltr` package installed in the target venv: the session
//! has Metal CB events yet zero sidecar events, so `breakdown` is 100 %
//! `<unscoped>` with nothing pointing at the missing package. The #163
//! lazy-eval notice cannot fire either — it keys off eval windows and there
//! are none at all. Detect this shape and say plainly how to fix it.

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use crate::rule::Rule;
use smeltr_core::event::{Event, Payload};

/// Detected "Metal capture without sidecar" shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarAbsent {
    /// Number of completed Metal CBs (GPU work was captured).
    pub metal_cb_count: u64,
    /// seq / ts of the first completed CB.
    pub first_seq: u64,
    pub first_ts_mono_ns: u64,
}

impl SidecarAbsent {
    /// Canonical explanation, shared by the CLI notice, the analyze finding
    /// detail and the MCP response.
    pub fn advice(&self) -> String {
        format!(
            "Metal capture recorded {} command buffer(s) but the Python \
             sidecar never attached (no PythonSidecarHello, module or eval \
             events), so nothing can be attributed to modules or scopes. If \
             the target is a Python/MLX workload, install the `smeltr` \
             package in ITS environment (`pip install -e python/` from the \
             smeltr repo) — `smeltr record` then auto-attaches it and \
             enables module/scope attribution, eval windows and `smeltr \
             origins`. For pure Metal/C++ targets this is expected.",
            self.metal_cb_count
        )
    }
}

/// Returns `Some` when the session contains completed Metal CBs but no
/// Python-sidecar event at all (hello, module call or mx.eval).
pub fn detect(events: &[Event]) -> Option<SidecarAbsent> {
    let mut metal_cb_count: u64 = 0;
    let mut first: Option<(u64, u64)> = None;
    for ev in events {
        match &ev.payload {
            Payload::PythonSidecarHello { .. }
            | Payload::ModuleEntered { .. }
            | Payload::MlxEvalEntered { .. } => return None,
            Payload::MetalCbCompleted { .. } => {
                metal_cb_count += 1;
                first.get_or_insert((ev.seq, ev.ts_mono_ns));
            }
            _ => {}
        }
    }
    let (first_seq, first_ts_mono_ns) = first?;
    Some(SidecarAbsent {
        metal_cb_count,
        first_seq,
        first_ts_mono_ns,
    })
}

pub struct SidecarAbsentRule;

impl Rule for SidecarAbsentRule {
    fn name(&self) -> &'static str {
        "sidecar_absent"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        let Some(absent) = detect(events) else {
            return Vec::new();
        };
        vec![Finding::new(
            Severity::Info,
            Category::ContributingFactor,
            "Python sidecar never attached: GPU time cannot be attributed to modules/scopes",
        )
        .with_detail(absent.advice())
        .with_evidence(EvidenceRef {
            seq: absent.first_seq,
            ts_mono_ns: absent.first_ts_mono_ns,
            description: format!(
                "first of {} completed Metal CB(s) in a session with zero \
                 Python-sidecar events",
                absent.metal_cb_count
            ),
        })]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::test_helpers::ev;
    use smeltr_core::event::Source;

    fn cb_completed(ts: u64, cb_id: u64) -> Event {
        ev(
            ts,
            Source::MetalHook,
            Payload::MetalCbCompleted {
                cb_id,
                queue_id: 1,
                status: 4,
                error_code: None,
                error_domain: None,
                in_flight_ns: 1_000,
            },
        )
    }

    #[test]
    fn metal_only_session_is_detected() {
        let events = vec![cb_completed(100, 1), cb_completed(200, 2)];
        let absent = detect(&events).expect("should detect");
        assert_eq!(absent.metal_cb_count, 2);
        assert_eq!(absent.first_seq, 100);
        assert!(absent.advice().contains("pip install"));
    }

    #[test]
    fn any_sidecar_event_suppresses_detection() {
        let hello = ev(
            10,
            Source::PythonSidecar,
            Payload::PythonSidecarHello {
                python_version: "3.12".into(),
                mlx_version: None,
                argv: vec![],
            },
        );
        let module = ev(
            10,
            Source::PythonSidecar,
            Payload::ModuleEntered {
                module_call_id: 1,
                module_def_id: 1,
                qualname: "A".into(),
                class_name: "A".into(),
                parent_call_id: None,
                depth: 0,
                fields: Default::default(),
            },
        );
        let eval = ev(
            10,
            Source::PythonSidecar,
            Payload::MlxEvalEntered {
                call_id: 1,
                array_count: 1,
                stream: "gpu".into(),
                module_stack: vec![],
                stack_frames: vec![],
            },
        );
        for sidecar_ev in [hello, module, eval] {
            let events = vec![sidecar_ev, cb_completed(100, 1)];
            assert!(detect(&events).is_none());
        }
    }

    #[test]
    fn no_metal_work_is_not_reported() {
        // No CBs at all (hook skipped, e.g. hardened binary): nothing to say.
        assert!(detect(&[]).is_none());
        let events = vec![ev(
            1,
            Source::MetalHook,
            Payload::MetalHookSkipped {
                reason: "hardened binary".into(),
            },
        )];
        assert!(detect(&events).is_none());
    }

    #[test]
    fn rule_emits_info_finding_with_advice() {
        let findings = SidecarAbsentRule.check(&[cb_completed(100, 1)]);
        assert_eq!(findings.len(), 1, "got: {findings:#?}");
        assert_eq!(findings[0].severity, Severity::Info);
        assert!(findings[0].detail.contains("pip install"));
        assert_eq!(findings[0].evidence.len(), 1);
    }
}

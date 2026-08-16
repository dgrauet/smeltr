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

/// Detected when the session holds neither Metal capture nor sidecar events:
/// typically the daemon's ambient session, which records system probes only.
/// Distinct from [`SidecarAbsent`], which presumes captured GPU work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NothingInstrumented {
    /// Total number of events in the session (all of them from probes).
    pub event_count: usize,
    /// Does the session carry process footprint samples?
    ///
    /// Ambient sessions NEVER carry any: their presence proves a process was
    /// genuinely followed, hence that a run was recorded.
    pub has_proc_footprint: bool,
}

impl NothingInstrumented {
    pub fn advice(&self) -> String {
        // Do NOT assert which kind of session this is when `ProcFootprint`
        // samples are present: `smeltr record --no-hook -- /bin/sleep 2`
        // produces exactly this shape in a SCOPED session. Telling someone who
        // just recorded a run that they are looking at the ambient session —
        // and advising them to record a run — is wrong twice over. Stick to
        // what is known.
        let rest = if self.has_proc_footprint {
            "A process was genuinely followed (footprint samples are present), \
             but neither the Metal hook nor the Python sidecar produced any \
             event: launched with `--no-hook`, against a non-Metal target, or \
             without the `smeltr` package installed in the target's Python \
             environment."
        } else {
            "This is the expected shape of the ambient session the daemon \
             opens at every startup. To analyze a real run, record it with \
             `smeltr record -- <command>`, then target it with `--last` or \
             with the name given through SMELTR_SESSION_NAME."
        };
        format!(
            "This session holds {} event(s), all of them from system probes: \
             no Metal capture and no Python sidecar event. There is therefore \
             nothing to break down by scope or by module — the empty arrays \
             mean \"nothing was instrumented\", not \"nothing to report\". \
             {rest}",
            self.event_count
        )
    }
}

/// Returns `Some` when the session holds no `MetalCbCompleted` and no Python
/// sidecar event, but at least one event overall.
pub fn detect_nothing_instrumented(events: &[Event]) -> Option<NothingInstrumented> {
    if events.is_empty() {
        return None;
    }
    let mut has_proc_footprint = false;
    for ev in events {
        match &ev.payload {
            Payload::PythonSidecarHello { .. }
            | Payload::ModuleEntered { .. }
            | Payload::MlxEvalEntered { .. }
            | Payload::MetalCbCompleted { .. } => return None,
            Payload::ProcFootprint { .. } => has_proc_footprint = true,
            _ => {}
        }
    }
    Some(NothingInstrumented {
        event_count: events.len(),
        has_proc_footprint,
    })
}

pub struct SidecarAbsentRule;

impl Rule for SidecarAbsentRule {
    fn name(&self) -> &'static str {
        "sidecar_absent"
    }

    fn check(&self, events: &[Event]) -> Vec<Finding> {
        if let Some(nothing) = detect_nothing_instrumented(events) {
            return vec![Finding::new(
                Severity::Info,
                Category::ContributingFactor,
                "No instrumented GPU workload in this session",
            )
            .with_detail(nothing.advice())];
        }
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

    fn vm_sample(ts: u64) -> Event {
        ev(
            ts,
            Source::Vm,
            Payload::VmSample {
                wired_bytes: 1,
                active_bytes: 1,
                compressed_bytes: 0,
                swap_used_bytes: 0,
                page_outs_per_sec: 0.0,
            },
        )
    }

    #[test]
    fn system_only_session_is_nothing_instrumented() {
        // The daemon's ambient session: system probes only.
        let events = vec![vm_sample(10), vm_sample(20)];
        let n = detect_nothing_instrumented(&events).expect("should detect");
        assert_eq!(n.event_count, 2);
        assert!(n.advice().contains("--last"));
    }

    #[test]
    fn metal_work_suppresses_nothing_instrumented() {
        // Il y a du Metal : c'est le cas #178, pas celui-ci.
        let events = vec![vm_sample(10), cb_completed(100, 1)];
        assert!(detect_nothing_instrumented(&events).is_none());
    }

    #[test]
    fn sidecar_events_suppress_nothing_instrumented() {
        let events = vec![ev(
            10,
            Source::PythonSidecar,
            Payload::PythonSidecarHello {
                python_version: "3.12".into(),
                mlx_version: None,
                argv: vec![],
            },
        )];
        assert!(detect_nothing_instrumented(&events).is_none());
    }

    fn proc_footprint(ts: u64) -> Event {
        ev(
            ts,
            Source::Proc,
            Payload::ProcFootprint {
                pid: 4242,
                name: "sleep".into(),
                phys_footprint_bytes: 1_000_000,
                lifetime_max_phys_footprint_bytes: 1_000_000,
                is_traced_root: true,
            },
        )
    }

    /// This branch is the counter-example to the old wording:
    /// `smeltr record --no-hook -- /bin/sleep 2` produces a SCOPED session
    /// holding nothing but `ProcFootprint` samples. Telling a user who just
    /// recorded a run that they are looking at the daemon's ambient session —
    /// and advising them to record a run — is wrong twice over.
    ///
    /// Ambient sessions never carry `ProcFootprint`: their presence settles it.
    #[test]
    fn a_session_with_footprint_samples_is_not_called_ambient() {
        let events = vec![proc_footprint(10), proc_footprint(20)];
        let n = detect_nothing_instrumented(&events).expect("should detect");
        let advice = n.advice();
        assert!(!advice.contains("ambient"), "advice: {advice}");
        // What is genuinely known still gets said.
        assert!(advice.contains("Metal"), "advice: {advice}");
        assert!(advice.contains("sidecar"), "advice: {advice}");
    }

    /// Without `ProcFootprint`, the shape remains that of the ambient session
    /// and the advice to record a run keeps its meaning.
    #[test]
    fn a_session_without_footprint_samples_still_mentions_the_ambient_shape() {
        let n = detect_nothing_instrumented(&[vm_sample(10)]).expect("should detect");
        assert!(n.advice().contains("ambient"), "advice: {}", n.advice());
    }

    #[test]
    fn empty_session_is_not_reported() {
        // Zero events: nothing to say, not even that nothing is instrumented.
        assert!(detect_nothing_instrumented(&[]).is_none());
    }

    /// Truth table of the three shapes, so nobody conflates them while
    /// refactoring.
    #[test]
    fn the_two_detectors_are_mutually_exclusive() {
        let system_only = vec![vm_sample(10)];
        let metal_no_sidecar = vec![cb_completed(100, 1)];

        assert!(detect(&system_only).is_none());
        assert!(detect_nothing_instrumented(&system_only).is_some());

        assert!(detect(&metal_no_sidecar).is_some());
        assert!(detect_nothing_instrumented(&metal_no_sidecar).is_none());
    }

    #[test]
    fn rule_emits_nothing_instrumented_finding() {
        let findings = SidecarAbsentRule.check(&[vm_sample(10)]);
        assert_eq!(findings.len(), 1, "got: {findings:#?}");
        assert_eq!(findings[0].severity, Severity::Info);
        assert!(findings[0].title.contains("instrument"));
    }
}

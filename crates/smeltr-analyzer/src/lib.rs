//! Deterministic analyzer for smeltr sessions.

pub mod finding;
pub mod report;
pub mod rule;
pub mod rules;

pub mod breakdown;
pub use breakdown::{
    aggregate_ops_flat, apply_op_group_by, compute as compute_breakdown, degraded_advice,
    prune_by_field_filter, render_chrome_trace, render_ops_flat, render_table, AttributionGap,
    BreakdownError, BreakdownNotices, Diagnostics, ModuleBreakdown, OpAttribution, OpFlatRow,
    OpGroupBy,
};

pub mod crash_join;
pub mod diff;

pub mod dispatch_origins;

pub mod export;

pub mod footprint;
pub mod memory;

pub mod op_clamp;
pub mod op_kinds;
pub mod windows;
pub use op_kinds::resolve_kind;

pub use finding::{Category, EvidenceRef, Finding, Severity};
pub use report::Report;
pub use rule::Rule;

use smeltr_core::event::Event;

pub fn analyze(events: &[Event]) -> Report {
    let mut report = Report {
        findings: Vec::new(),
        session_short: events.first().map(|e| {
            let s = e.session_id.as_simple().to_string();
            s[..s.len().min(8)].to_string()
        }),
        event_count: events.len(),
    };
    for rule in rule::all_rules() {
        report.findings.extend(rule.check(events));
    }
    report
}

/// The report every surface shows for the session stored in `dir`, whose
/// `events` the caller has already read: the rules, plus the retroactive
/// crash and jetsam joins (#153, #200), named after the session analyzed.
///
/// One function so no surface can skip a join again: `smeltr analyze` and
/// `get_session_summary` did them while `list_sessions` and
/// `compare_sessions` did not, and reported no root cause for a crashed
/// run (#204, #242).
pub fn analyze_session(dir: &std::path::Path, events: &[Event]) -> Report {
    let mut report = analyze(events);
    if let Ok(meta) = smeltr_core::reader::read_metadata(dir) {
        // #170: post-mortem sessions carry events stamped with the ambient
        // session that ingested them — name the session actually analyzed.
        report.session_short = Some(meta.session_id.short());
    }
    crash_join::join_crash(&mut report, dir);
    crash_join::join_jetsam(&mut report, dir);
    report
}

/// Title prefix of the finding that flags a damaged or truncated session.
pub const INCOMPLETE_DATA_TITLE: &str = "Session data is incomplete";

/// [`analyze_session`] over what [`smeltr_core::reader::read_events_checked`]
/// returned: when the event file is damaged, the report opens with a finding
/// saying so, and conclusions that only an absence of events supports are
/// withdrawn — a truncated file used to yield "No instrumented GPU workload"
/// (#268).
pub fn analyze_session_checked(
    dir: &std::path::Path,
    events: &[Event],
    damage: Option<&smeltr_core::reader::Damage>,
) -> Report {
    let mut report = analyze_session(dir, events);
    if let Some(damage) = damage {
        report
            .findings
            .retain(|f| f.title != rules::sidecar_absent::NOTHING_INSTRUMENTED_TITLE);
        report.findings.insert(
            0,
            Finding::new(
                Severity::Warning,
                Category::ContributingFactor,
                format!(
                    "{INCOMPLETE_DATA_TITLE}: this report covers only the {} event(s) that could be read",
                    events.len()
                ),
            )
            .with_detail(format!(
                "{damage}. Findings based on the absence of an event (no GPU work, no sidecar, \
                 no crash) may be wrong."
            )),
        );
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_events_yields_empty_report() {
        let r = analyze(&[]);
        assert!(r.findings.is_empty());
        assert_eq!(r.event_count, 0);
        assert!(r.session_short.is_none());
    }

    #[test]
    fn render_handles_empty_report() {
        let r = analyze(&[]);
        let text = r.render();
        assert!(text.contains("=== smeltr analyze ==="));
        assert!(text.contains("events:  0"));
    }
}

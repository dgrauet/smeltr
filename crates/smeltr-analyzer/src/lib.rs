//! Deterministic analyzer for smeltr sessions.

pub mod finding;
pub mod report;
pub mod rule;
pub mod rules;

pub mod breakdown;
pub use breakdown::{
    compute as compute_breakdown, render_chrome_trace, render_ops_flat, render_table,
    BreakdownError, Diagnostics, ModuleBreakdown, OpAttribution,
};

pub mod diff;

pub mod export;

pub mod memory;

pub mod op_kinds;
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

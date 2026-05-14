//! Aggregated analyzer output.

use crate::finding::{Category, Finding};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Report {
    pub findings: Vec<Finding>,
    pub session_short: Option<String>,
    pub event_count: usize,
}

impl Report {
    pub fn root_cause(&self) -> Option<&Finding> {
        self.findings
            .iter()
            .find(|f| f.category == Category::RootCause)
    }

    pub fn contributing_factors(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.category == Category::ContributingFactor)
    }

    pub fn timing(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.category == Category::Timing)
    }

    pub fn system_pressure(&self) -> impl Iterator<Item = &Finding> {
        self.findings
            .iter()
            .filter(|f| f.category == Category::SystemPressure)
    }

    /// Renders the report in the format described in spec section 5.2.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("=== smeltr analyze ===\n");
        if let Some(s) = &self.session_short {
            out.push_str(&format!("session: {}\n", s));
        }
        out.push_str(&format!("events:  {}\n", self.event_count));
        out.push('\n');

        if let Some(rc) = self.root_cause() {
            out.push_str("ROOT CAUSE\n");
            out.push_str(&format!("  [{:?}] {}\n", rc.severity, rc.title));
            if !rc.detail.is_empty() {
                out.push_str(&format!("    {}\n", rc.detail));
            }
            out.push('\n');
        }

        let cfs: Vec<_> = self.contributing_factors().collect();
        if !cfs.is_empty() {
            out.push_str("CONTRIBUTING FACTORS\n");
            for f in cfs {
                out.push_str(&format!("  [{:?}] {}\n", f.severity, f.title));
                if !f.detail.is_empty() {
                    out.push_str(&format!("    {}\n", f.detail));
                }
            }
            out.push('\n');
        }

        let timings: Vec<_> = self.timing().collect();
        if !timings.is_empty() {
            out.push_str("TIMING\n");
            for f in timings {
                out.push_str(&format!("  {}\n", f.title));
                if !f.detail.is_empty() {
                    out.push_str(&format!("    {}\n", f.detail));
                }
            }
            out.push('\n');
        }

        let pressures: Vec<_> = self.system_pressure().collect();
        if !pressures.is_empty() {
            out.push_str("SYSTEM PRESSURE\n");
            for f in pressures {
                out.push_str(&format!("  [{:?}] {}\n", f.severity, f.title));
                if !f.detail.is_empty() {
                    out.push_str(&format!("    {}\n", f.detail));
                }
            }
            out.push('\n');
        }
        out
    }
}

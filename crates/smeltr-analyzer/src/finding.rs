//! Findings produced by analyzer rules.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Category {
    RootCause,
    ContributingFactor,
    Timing,
    SystemPressure,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub seq: u64,
    pub ts_mono_ns: u64,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub category: Category,
    pub title: String,
    pub detail: String,
    pub evidence: Vec<EvidenceRef>,
}

impl Finding {
    pub fn new(severity: Severity, category: Category, title: impl Into<String>) -> Self {
        Self {
            severity,
            category,
            title: title.into(),
            detail: String::new(),
            evidence: Vec::new(),
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn with_evidence(mut self, ev: EvidenceRef) -> Self {
        self.evidence.push(ev);
        self
    }
}

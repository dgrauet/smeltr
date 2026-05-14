//! `compare_sessions` tool: side-by-side stats of two sessions.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_core::event::Source;
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    pub session_a: String,
    pub session_b: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub a: SessionStats,
    pub b: SessionStats,
    pub delta: DeltaStats,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionStats {
    pub session_id: String,
    pub event_count: usize,
    pub duration_ns: u64,
    pub source_counts: HashMap<String, usize>,
    pub root_cause_title: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeltaStats {
    pub event_count_diff: i64,
    pub duration_diff_ns: i64,
    pub root_cause_match: bool,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let a = stats(&params.session_a)?;
    let b = stats(&params.session_b)?;
    let event_count_diff = b.event_count as i64 - a.event_count as i64;
    let duration_diff_ns = b.duration_ns as i64 - a.duration_ns as i64;
    let root_cause_match = a.root_cause_title == b.root_cause_title;
    Ok(Response {
        a,
        b,
        delta: DeltaStats {
            event_count_diff,
            duration_diff_ns,
            root_cause_match,
        },
    })
}

fn stats(arg: &str) -> Result<SessionStats, ToolError> {
    let dir = resolve_session(arg)?;
    let events = smeltr_core::reader::read_events(&dir)?;
    let duration_ns = if events.len() < 2 {
        0
    } else {
        events
            .last()
            .unwrap()
            .ts_mono_ns
            .saturating_sub(events.first().unwrap().ts_mono_ns)
    };
    let mut counts: HashMap<String, usize> = HashMap::new();
    for ev in &events {
        *counts.entry(source_str(&ev.source).into()).or_insert(0) += 1;
    }
    let report = smeltr_analyzer::analyze(&events);
    let root_cause_title = report.root_cause().map(|f| f.title.clone());
    Ok(SessionStats {
        session_id: dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string(),
        event_count: events.len(),
        duration_ns,
        source_counts: counts,
        root_cause_title,
    })
}

fn source_str(s: &Source) -> &'static str {
    match s {
        Source::Mark => "Mark",
        Source::System => "System",
        Source::IoReport => "IoReport",
        Source::Vm => "Vm",
        Source::Proc => "Proc",
        Source::OsLog => "OsLog",
        Source::Thermal => "Thermal",
        Source::MachExc => "MachExc",
        Source::CrashReport => "CrashReport",
        Source::MetalHook => "MetalHook",
        Source::PythonSidecar => "PythonSidecar",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Event, Payload};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    fn make_session(label: &str) -> SessionId {
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        w.write_event(&Event {
            ts_mono_ns: 1_000_000_000,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: 1,
            payload: Payload::Mark {
                label: label.into(),
            },
        })
        .unwrap();
        w.finalize(Some(0), "x".into()).unwrap();
        id
    }

    #[test]
    #[serial_test::serial]
    fn compares_two_sessions() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let a = make_session("a");
        let b = make_session("b");
        let resp = run(Params {
            session_a: a.short(),
            session_b: b.short(),
        })
        .unwrap();
        assert!(resp.a.event_count >= 1);
        assert!(resp.b.event_count >= 1);
        assert!(resp.delta.root_cause_match); // Both have no root cause -> match.
    }
}

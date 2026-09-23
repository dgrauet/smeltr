//! `list_sessions` tool: enumerate sessions on disk.

use crate::types::ToolError;
use serde::{Deserialize, Serialize};
use smeltr_core::reader::{read_events, read_metadata};
use smeltr_core::session_resolve::sessions_newest_first;

const MIN_USEFUL_EVENT_COUNT: usize = 20;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct Params {
    /// When false (default), sessions with fewer than 20 events are
    /// excluded from the listing as likely-orphan daemon-spawn sessions
    /// without workload. Set to true to include them.
    pub include_empty: Option<bool>,
    /// Page size (default 50). Sessions come newest first by start time.
    pub limit: Option<usize>,
    /// Sessions to skip, counted after the `include_empty` filter — pass
    /// the previous response's `next_offset`.
    pub offset: Option<usize>,
}

/// Default page size: a store of 272 sessions used to come back in one
/// 74k-character response, past an MCP client's tool-output limit (#261).
const DEFAULT_LIMIT: usize = 50;

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub sessions: Vec<SessionSummary>,
    /// Offset of the next page, when more sessions remain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionSummary {
    pub short_id: String,
    pub full_id: String,
    pub dir_name: String,
    pub started_rfc3339: String,
    pub ended_rfc3339: Option<String>,
    pub exit_code: Option<i32>,
    pub event_count: usize,
    pub root_cause_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    // Newest first by start time: by directory name the oldest came first
    // and every post-mortem after every recording (#261).
    let dirs = sessions_newest_first()?;
    let include_empty = params.include_empty.unwrap_or(false);
    // At least 1: an empty page pointing at its own offset would loop a
    // paging client forever.
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).max(1);
    let offset = params.offset.unwrap_or(0);
    let mut out = Vec::with_capacity(limit.min(dirs.len()));
    let mut matched = 0usize;
    let mut next_offset = None;
    for dir in dirs.iter() {
        let dir_name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let meta = read_metadata(dir).ok();
        let events = read_events(dir).unwrap_or_default();

        if !include_empty && events.len() < MIN_USEFUL_EVENT_COUNT {
            continue;
        }
        matched += 1;
        if matched <= offset {
            continue;
        }
        if out.len() == limit {
            // One more match exists past this page; only the page itself
            // pays for the analysis below.
            next_offset = Some(offset + limit);
            break;
        }

        let report = smeltr_analyzer::analyze_session(dir, &events);
        let root_cause_title = report.root_cause().map(|f| f.title.clone());
        let (full_id, started, ended, exit_code, name) = match &meta {
            Some(m) => (
                m.session_id.to_string(),
                m.started_rfc3339.clone(),
                m.ended_rfc3339.clone(),
                m.exit_code,
                m.name.clone(),
            ),
            None => (String::new(), String::new(), None, None, None),
        };
        let short_id = if full_id.len() >= 8 {
            full_id[..8].to_string()
        } else {
            full_id.clone()
        };
        out.push(SessionSummary {
            short_id,
            full_id,
            dir_name,
            started_rfc3339: started,
            ended_rfc3339: ended,
            exit_code,
            event_count: events.len(),
            root_cause_title,
            name,
        });
    }
    Ok(Response {
        sessions: out,
        next_offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Event, Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    fn make_session(events: &[Event]) {
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        for e in events {
            w.write_event(e).unwrap();
        }
        w.finalize(Some(0), "2026-05-14T00:00:00Z".into()).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn empty_home_returns_empty_list() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let resp = run(Params::default()).unwrap();
        assert!(resp.sessions.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn lists_one_session_with_root_cause_from_analyzer() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        make_session(&[Event {
            ts_mono_ns: 1,
            ts_wall_ns: 1,
            session_id: Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq: 1,
            payload: Payload::MetalCbCompleted {
                cb_id: 1,
                queue_id: 1,
                status: 4,
                error_code: Some(14),
                error_domain: Some("IOGPU".into()),
                in_flight_ns: 1,
            },
        }]);
        let resp = run(Params {
            include_empty: Some(true),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resp.sessions.len(), 1);
        let s = &resp.sessions[0];
        assert_eq!(s.event_count, 1);
        assert!(s
            .root_cause_title
            .as_ref()
            .unwrap()
            .contains("ImpactingInteractivity"));
    }

    #[test]
    #[serial_test::serial]
    fn excludes_orphan_sessions_by_default() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());

        // Session 1: minimal (just finalize, no user events).
        let id1 = SessionId::new();
        let meta1 = SessionMetadata::now_starting(id1);
        let w1 = SessionWriter::create(meta1).unwrap();
        w1.finalize(Some(0), "x".into()).unwrap();

        // Session 2: substantial (30 marks).
        let id2 = SessionId::new();
        let meta2 = SessionMetadata::now_starting(id2);
        let mut w2 = SessionWriter::create(meta2).unwrap();
        for i in 0..30 {
            w2.write_event(&Event {
                ts_mono_ns: i,
                ts_wall_ns: i,
                session_id: Uuid::nil(),
                source: Source::Mark,
                pid: None,
                seq: i,
                payload: Payload::Mark {
                    label: format!("m{i}"),
                    fields: Default::default(),
                },
            })
            .unwrap();
        }
        w2.finalize(Some(0), "x".into()).unwrap();

        // Default: orphan excluded, only the substantial session.
        let resp = run(Params::default()).unwrap();
        assert_eq!(resp.sessions.len(), 1);
        assert_eq!(resp.sessions[0].short_id, id2.short());

        // include_empty=true: both listed.
        let resp = run(Params {
            include_empty: Some(true),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resp.sessions.len(), 2);
    }

    #[test]
    #[serial_test::serial]
    fn lists_session_name_when_present() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.name = Some("ltx2-experiment".into());
        let mut w = SessionWriter::create(meta).unwrap();
        for i in 0..25 {
            w.write_event(&Event {
                ts_mono_ns: i,
                ts_wall_ns: i,
                session_id: Uuid::nil(),
                source: Source::Mark,
                pid: None,
                seq: i,
                payload: Payload::Mark {
                    label: format!("m{i}"),
                    fields: Default::default(),
                },
            })
            .unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params::default()).unwrap();
        assert_eq!(resp.sessions.len(), 1);
        assert_eq!(resp.sessions[0].name.as_deref(), Some("ltx2-experiment"));
    }

    #[test]
    #[serial_test::serial]
    fn lists_session_name_as_none_when_absent() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        for i in 0..25 {
            w.write_event(&Event {
                ts_mono_ns: i,
                ts_wall_ns: i,
                session_id: Uuid::nil(),
                source: Source::Mark,
                pid: None,
                seq: i,
                payload: Payload::Mark {
                    label: format!("m{i}"),
                    fields: Default::default(),
                },
            })
            .unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params::default()).unwrap();
        assert_eq!(resp.sessions[0].name, None);
    }

    /// A recorded run (the fixture's pid) that crashed, with its report in
    /// the test DiagnosticReports directory.
    fn crashed_run(reports: &std::path::Path) -> String {
        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.kind = smeltr_core::session::SessionKind::Scoped {
            pid: 11672,
            argv: vec!["python".into()],
        };
        drop(SessionWriter::create(meta).unwrap());
        std::fs::write(
            reports.join("python-2026-07-16.ips"),
            include_str!(
                "../../../smeltr-probes-crash-reports/tests/fixtures/sample_multiline.ips"
            ),
        )
        .unwrap();
        id.short()
    }

    /// #242: the listed root cause must match `get_session_summary`, which
    /// joins the crash report — a crashed run showed no root cause here.
    #[test]
    #[serial_test::serial]
    fn root_cause_includes_the_joined_crash_report() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());
        crashed_run(reports.path());
        let resp = run(Params {
            include_empty: Some(true),
            ..Default::default()
        });
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");
        let title = resp.unwrap().sessions[0].root_cause_title.clone();
        assert!(
            title
                .as_deref()
                .is_some_and(|t| t.starts_with("Recorded process crashed")),
            "got {title:?}"
        );
    }

    fn session_started(started: &str) -> String {
        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.started_rfc3339 = started.into();
        SessionWriter::create(meta)
            .unwrap()
            .finalize(Some(0), "ok".into())
            .unwrap();
        id.short()
    }

    fn page(limit: Option<usize>, offset: Option<usize>) -> Response {
        run(Params {
            include_empty: Some(true),
            limit,
            offset,
        })
        .unwrap()
    }

    /// #261: newest first by start time — a post-mortem sorted after every
    /// recording by directory name, and the oldest sessions came first.
    #[test]
    #[serial_test::serial]
    fn sessions_come_newest_first() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let old = session_started("2026-05-01T00:00:00Z");
        let new = session_started("2026-09-01T00:00:00Z");
        let mid = session_started("2026-07-01T00:00:00Z");
        let ids: Vec<String> = page(None, None)
            .sessions
            .into_iter()
            .map(|s| s.short_id)
            .collect();
        assert_eq!(ids, vec![new, mid, old]);
    }

    /// #261: 272 sessions came back in one 74k-character response, over
    /// the MCP client's tool-output limit.
    #[test]
    #[serial_test::serial]
    fn sessions_are_paged() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let ids: Vec<String> = (1..=5)
            .map(|d| session_started(&format!("2026-09-0{d}T00:00:00Z")))
            .rev()
            .collect();
        let first = page(Some(2), None);
        assert_eq!(first.sessions.len(), 2);
        assert_eq!(first.sessions[0].short_id, ids[0]);
        assert_eq!(first.next_offset, Some(2));
        let last = page(Some(2), Some(4));
        assert_eq!(last.sessions.len(), 1);
        assert_eq!(last.sessions[0].short_id, ids[4]);
        assert_eq!(last.next_offset, None);
    }

    #[test]
    #[serial_test::serial]
    fn default_page_holds_fifty() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        for i in 0..55 {
            session_started(&format!("2026-09-01T00:{:02}:{:02}Z", i / 60, i % 60));
        }
        let resp = page(None, None);
        assert_eq!(resp.sessions.len(), 50);
        assert_eq!(resp.next_offset, Some(50));
    }
}

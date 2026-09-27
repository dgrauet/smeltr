//! `query_events` tool: filter session events by source/payload-kind/time.

use crate::types::{bounded_count, resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_core::event::{Event, Source};

/// ~270 characters per event: 100 events stay well inside an MCP client's
/// tool-output limit, where the former 1000 came back as 266k (#271).
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct Params {
    pub session: String,
    pub source: Option<String>,
    pub payload_kind: Option<String>,
    pub from_ts_mono_ns: Option<u64>,
    pub to_ts_mono_ns: Option<u64>,
    /// Max events returned (default 100, 1..=1000). Narrow with the
    /// filters, or page with `from_ts_mono_ns`, rather than raising it.
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub events: Vec<Event>,
    pub matched: usize,
    pub total: usize,
    pub truncated: bool,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let limit = bounded_count("limit", params.limit, DEFAULT_LIMIT, MAX_LIMIT)?;
    let filter = smeltr_core::EventFilter {
        source: match params.source.as_deref() {
            None => None,
            Some(s) => Some(parse_source(s)?),
        },
        from_ts: params.from_ts_mono_ns,
        to_ts: params.to_ts_mono_ns,
        payload_kind: params.payload_kind.clone(),
    };
    let dir = resolve_session(&params.session)?;

    // One pass that keeps only the page: the session used to be decoded
    // twice (once just to count it) and every match held in memory before
    // the truncation — 16.9 s and 1.6 GB for `limit: 5` on 3.3 M events
    // (#271). A chunked session's footer counts it without decoding, and
    // then the filter can skip whole chunks.
    let indexed_total = smeltr_core::reader::indexed_event_count(&dir)?;
    let pass_filter = indexed_total.is_some().then_some(&filter);
    let mut events = Vec::new();
    let (mut total, mut matched) = (0usize, 0usize);
    smeltr_core::reader::for_each_event(&dir, pass_filter, |e| {
        total += 1;
        if filter.matches(&e) {
            matched += 1;
            if events.len() < limit {
                events.push(e);
            }
        }
        std::ops::ControlFlow::Continue(())
    })?;
    Ok(Response {
        events,
        matched,
        total: indexed_total.unwrap_or(total),
        truncated: matched > limit,
    })
}

fn parse_source(s: &str) -> Result<Source, ToolError> {
    Ok(match s {
        "Mark" => Source::Mark,
        "System" => Source::System,
        "IoReport" => Source::IoReport,
        "Vm" => Source::Vm,
        "Proc" => Source::Proc,
        "OsLog" => Source::OsLog,
        "Thermal" => Source::Thermal,
        "MachExc" => Source::MachExc,
        "CrashReport" => Source::CrashReport,
        "MetalHook" => Source::MetalHook,
        "PythonSidecar" => Source::PythonSidecar,
        other => return Err(ToolError::BadArgs(format!("unknown source {other:?}"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::Payload;
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    fn write_session() -> SessionId {
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        for i in 0..5 {
            w.write_event(&Event {
                ts_mono_ns: i * 100,
                ts_wall_ns: 0,
                session_id: Uuid::nil(),
                source: Source::Mark,
                pid: None,
                seq: i,
                payload: Payload::Mark {
                    label: format!("m-{i}"),
                    fields: Default::default(),
                },
            })
            .unwrap();
        }
        w.write_event(&Event {
            ts_mono_ns: 1000,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq: 99,
            payload: Payload::MetalCbCommitted {
                cb_id: 1,
                queue_id: 1,
                queue_depth: 1,
                label: None,
            },
        })
        .unwrap();
        w.finalize(Some(0), "x".into()).unwrap();
        id
    }

    #[test]
    #[serial_test::serial]
    fn filter_by_source_returns_only_matching() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = write_session();
        let resp = run(Params {
            session: id.short(),
            source: Some("Mark".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resp.matched, 5);
        assert!(resp.events.iter().all(|e| e.source == Source::Mark));
    }

    /// `total` is the whole session and `matched` the filter's hits, both
    /// counted in the single pass (legacy) or from the footer (chunked).
    #[test]
    #[serial_test::serial]
    fn counts_total_and_matched_in_both_formats() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        for chunked in [false, true] {
            let id = SessionId::new();
            let cfg = chunked.then_some(smeltr_core::chunked::ChunkConfig {
                max_events: 4,
                max_bytes: smeltr_core::chunked::CHUNK_BYTES,
                flush_min_bytes: smeltr_core::chunked::FLUSH_MIN_BYTES,
            });
            let mut w =
                SessionWriter::create_with_chunk_config(SessionMetadata::now_starting(id), cfg)
                    .unwrap();
            for i in 0..30u64 {
                w.write_event(&Event {
                    ts_mono_ns: i,
                    ts_wall_ns: 0,
                    session_id: Uuid::nil(),
                    source: if i < 10 { Source::Mark } else { Source::Vm },
                    pid: None,
                    seq: i,
                    payload: Payload::Mark {
                        label: format!("m-{i}"),
                        fields: Default::default(),
                    },
                })
                .unwrap();
            }
            w.finalize(Some(0), "x".into()).unwrap();
            let resp = run(Params {
                session: id.short(),
                source: Some("Mark".into()),
                limit: Some(3),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(resp.total, 30, "chunked {chunked}");
            assert_eq!(resp.matched, 10, "chunked {chunked}");
            assert_eq!(
                resp.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
                vec![0, 1, 2]
            );
            assert!(resp.truncated);
        }
    }

    #[test]
    #[serial_test::serial]
    fn filter_by_payload_kind() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = write_session();
        let resp = run(Params {
            session: id.short(),
            payload_kind: Some("MetalCbCommitted".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resp.matched, 1);
    }

    /// #271: the default of 1000 events came back as 266k characters, far
    /// past an MCP client's tool-output limit.
    #[test]
    #[serial_test::serial]
    fn default_page_is_100_events() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let mut w = SessionWriter::create(SessionMetadata::now_starting(id)).unwrap();
        for i in 0..150 {
            w.write_event(&Event {
                ts_mono_ns: i,
                ts_wall_ns: 0,
                session_id: Uuid::nil(),
                source: Source::Mark,
                pid: None,
                seq: i,
                payload: Payload::Mark {
                    label: format!("m-{i}"),
                    fields: Default::default(),
                },
            })
            .unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();
        let resp = run(Params {
            session: id.short(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resp.events.len(), 100);
        assert_eq!(resp.matched, 150);
        assert!(resp.truncated);
    }

    /// #271: `limit: 100000` returned 25 M characters and `limit: 0` was
    /// served silently. Both are refused, before the session is read.
    #[test]
    #[serial_test::serial]
    fn out_of_range_limit_is_bad_args() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        for limit in [0, 1001, 100_000] {
            let r = run(Params {
                session: "deadbeef".into(),
                limit: Some(limit),
                ..Default::default()
            });
            assert!(
                matches!(&r, Err(ToolError::BadArgs(m)) if m.contains("1000")),
                "limit {limit}: {r:?}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn limit_truncates() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = write_session();
        let resp = run(Params {
            session: id.short(),
            limit: Some(2),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resp.events.len(), 2);
        assert!(resp.matched > 2);
        assert!(resp.truncated);
    }
}

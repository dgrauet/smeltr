//! `get_metal_cb_history` tool: filter Metal events.

use crate::types::{bounded_count, resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_core::event::{Event, Payload};

const DEFAULT_LIMIT: usize = 100;
/// `limit: 100000` returned 24 M characters (#271).
const MAX_LIMIT: usize = 1000;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct Params {
    pub session: String,
    pub queue_id: Option<u64>,
    /// Max events returned (default 100, 1..=1000); page with `offset`.
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub events: Vec<Event>,
    pub matched: usize,
    pub total: usize,
    pub truncated: bool,
    pub offset: usize,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let limit = bounded_count("limit", params.limit, DEFAULT_LIMIT, MAX_LIMIT)?;
    let offset = params.offset.unwrap_or(0);
    let dir = resolve_session(&params.session)?;
    // One streaming pass that keeps only the requested page (#271).
    let mut events = Vec::new();
    let (mut total, mut matched) = (0usize, 0usize);
    crate::session_cache::visit(&dir, None, |e| {
        total += 1;
        let wanted = is_metal(&e.payload)
            && params
                .queue_id
                .is_none_or(|want| payload_queue_id(&e.payload) == Some(want));
        if wanted {
            if matched >= offset && events.len() < limit {
                events.push(e.clone());
            }
            matched += 1;
        }
        std::ops::ControlFlow::Continue(())
    })?;

    Ok(Response {
        events,
        matched,
        total,
        truncated: matched > offset.saturating_add(limit),
        offset,
    })
}

fn is_metal(p: &Payload) -> bool {
    matches!(
        p,
        Payload::MetalCbCommitted { .. }
            | Payload::MetalCbScheduled { .. }
            | Payload::MetalCbCompleted { .. }
            | Payload::MetalCbWarning { .. }
            | Payload::MetalHeapAlloc { .. }
            | Payload::MetalHeapFree { .. }
            | Payload::MetalBufferAlloc { .. }
            | Payload::MetalBufferFree { .. }
            | Payload::MetalTextureAlloc { .. }
            | Payload::MetalTextureFree { .. }
            | Payload::MetalHookDropped { .. }
            | Payload::MetalHookSkipped { .. }
    )
}

fn payload_queue_id(p: &Payload) -> Option<u64> {
    match p {
        Payload::MetalCbCommitted { queue_id, .. }
        | Payload::MetalCbScheduled { queue_id, .. }
        | Payload::MetalCbCompleted { queue_id, .. }
        | Payload::MetalCbWarning { queue_id, .. } => Some(*queue_id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::Source;
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    #[test]
    #[serial_test::serial]
    fn filters_by_queue_id() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        for (i, qid) in [(1u64, 1u64), (2, 2), (3, 1)] {
            w.write_event(&Event {
                ts_mono_ns: i,
                ts_wall_ns: i,
                session_id: Uuid::nil(),
                source: Source::MetalHook,
                pid: None,
                seq: i,
                payload: Payload::MetalCbCommitted {
                    cb_id: i,
                    queue_id: qid,
                    queue_depth: 1,
                    label: None,
                },
            })
            .unwrap();
        }
        w.write_event(&Event {
            ts_mono_ns: 4,
            ts_wall_ns: 4,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: 4,
            payload: Payload::Mark {
                label: "not-metal".into(),
                fields: Default::default(),
            },
        })
        .unwrap();
        w.finalize(Some(0), "2026-05-14T00:00:00Z".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
            queue_id: Some(1),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resp.matched, 2);
        assert!(resp.total >= 4);
    }

    #[test]
    #[serial_test::serial]
    fn no_filter_returns_all_metal() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        w.write_event(&Event {
            ts_mono_ns: 1,
            ts_wall_ns: 1,
            session_id: Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq: 1,
            payload: Payload::MetalHeapAlloc {
                heap_id: 1,
                size_bytes: 1024,
                label: None,
            },
        })
        .unwrap();
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resp.matched, 1);
    }

    #[test]
    #[serial_test::serial]
    fn limit_truncates_history() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        for i in 1u64..=50 {
            w.write_event(&Event {
                ts_mono_ns: i,
                ts_wall_ns: i,
                session_id: Uuid::nil(),
                source: Source::MetalHook,
                pid: None,
                seq: i,
                payload: Payload::MetalCbCommitted {
                    cb_id: i,
                    queue_id: 1,
                    queue_depth: i as u32,
                    label: None,
                },
            })
            .unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
            limit: Some(10),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resp.events.len(), 10);
        assert_eq!(resp.matched, 50);
        assert!(resp.truncated);
        assert_eq!(resp.offset, 0);
    }

    /// #271: `limit: 100000` returned 24 M characters.
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
    fn offset_skips_initial_events() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        for i in 1u64..=10 {
            w.write_event(&Event {
                ts_mono_ns: i,
                ts_wall_ns: i,
                session_id: Uuid::nil(),
                source: Source::MetalHook,
                pid: None,
                seq: i,
                payload: Payload::MetalCbCommitted {
                    cb_id: i,
                    queue_id: 1,
                    queue_depth: i as u32,
                    label: None,
                },
            })
            .unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
            limit: Some(5),
            offset: Some(3),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resp.events.len(), 5);
        assert_eq!(resp.events[0].seq, 4); // 3 skipped
        assert_eq!(resp.offset, 3);
        assert!(resp.truncated, "seq 9 and 10 remain");
        assert_eq!(resp.matched, 10);

        let last = run(Params {
            session: id.short(),
            limit: Some(5),
            offset: Some(5),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            last.events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![6, 7, 8, 9, 10]
        );
        assert!(!last.truncated, "the last page");
    }
}

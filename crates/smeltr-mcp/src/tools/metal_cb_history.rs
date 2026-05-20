//! `get_metal_cb_history` tool: filter Metal events.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_core::event::{Event, Payload};

const DEFAULT_LIMIT: usize = 100;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct Params {
    pub session: String,
    pub queue_id: Option<u64>,
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
    let dir = resolve_session(&params.session)?;
    let events = smeltr_core::reader::read_events(&dir)?;
    let total = events.len();
    let filtered: Vec<Event> = events
        .into_iter()
        .filter(|e| is_metal(&e.payload))
        .filter(|e| match params.queue_id {
            None => true,
            Some(want) => payload_queue_id(&e.payload)
                .map(|q| q == want)
                .unwrap_or(false),
        })
        .collect();
    let matched = filtered.len();

    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
    let mut sliced: Vec<Event> = filtered.into_iter().skip(offset).collect();
    let truncated = sliced.len() > limit;
    sliced.truncate(limit);

    Ok(Response {
        events: sliced,
        matched,
        total,
        truncated,
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
    }
}

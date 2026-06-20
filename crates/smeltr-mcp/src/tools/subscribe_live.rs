//! `subscribe_live` tool: delta summary of a running session since a cursor.
//!
//! Poll-based "live tail": each call reads the session log from disk, slices
//! events after `cursor`, and returns a rolled-up delta. The cursor is an
//! event-sequence index (count consumed). See the design spec for why disk-tail
//! beats a live bus bridge for a turn-based LLM consumer.

use serde::{Deserialize, Serialize};
use smeltr_core::event::Event;
use std::collections::BTreeMap;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct Params {
    /// Session ref (short id / UUID / name). Omit to target the most-recent
    /// live session. Pass back the returned `session_id` on every later poll.
    pub session: Option<String>,
    /// Events already consumed. Omit/0 for a baseline summary from the start.
    pub cursor: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct PayloadCount {
    pub kind: String,
    pub count: u64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct GpuDelta {
    pub new_cbs: u64,
    pub gpu_ms_added: f64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct OpDelta {
    pub kind: String,
    pub count: u64,
    pub gpu_ms: f64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct MemDelta {
    pub current_bytes: u64,
    pub peak_bytes_session: u64,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelLoadDelta {
    pub name: String,
    pub bytes: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub session_id: String,
    pub session_name: Option<String>,
    pub live: bool,
    pub cursor: u64,
    pub prev_cursor: u64,
    pub new_events: u64,
    pub elapsed_ns: u64,
    pub by_payload: Vec<PayloadCount>,
    pub gpu: GpuDelta,
    pub top_ops: Vec<OpDelta>,
    pub memory: MemDelta,
    pub model_loads: Vec<ModelLoadDelta>,
    pub note: Option<String>,
}

pub fn summarize_delta(
    events: &[Event],
    cursor: u64,
    session_id: String,
    session_name: Option<String>,
    live: bool,
) -> Response {
    let len = events.len() as u64;
    let clamped = cursor > len;
    let cur = cursor.min(len) as usize;
    let delta = &events[cur..];
    let new_events = delta.len() as u64;

    let elapsed_ns = match (events.first(), events.last()) {
        (Some(a), Some(b)) => b.ts_mono_ns.saturating_sub(a.ts_mono_ns),
        _ => 0,
    };

    let mut counts: BTreeMap<&'static str, u64> = BTreeMap::new();
    for e in delta {
        *counts
            .entry(crate::tools::query_events::payload_kind(e))
            .or_insert(0) += 1;
    }
    let mut by_payload: Vec<PayloadCount> = counts
        .into_iter()
        .map(|(kind, count)| PayloadCount {
            kind: kind.to_string(),
            count,
        })
        .collect();
    by_payload.sort_by(|a, b| b.count.cmp(&a.count).then(a.kind.cmp(&b.kind)));

    let note = if clamped {
        Some("cursor ahead of log (clamped)".to_string())
    } else if new_events == 0 {
        Some(if live {
            "no new events".to_string()
        } else {
            "session finalized; no new events".to_string()
        })
    } else if !live {
        Some("session finalized".to_string())
    } else {
        None
    };

    Response {
        session_id,
        session_name,
        live,
        cursor: len,
        prev_cursor: cur as u64,
        new_events,
        elapsed_ns,
        by_payload,
        gpu: GpuDelta::default(),
        top_ops: Vec::new(),
        memory: MemDelta::default(),
        model_loads: Vec::new(),
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Payload, Source};
    use uuid::Uuid;

    fn mark(seq: u64, ts: u64) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq,
            payload: Payload::Mark {
                label: format!("m-{seq}"),
                fields: Default::default(),
            },
        }
    }

    #[test]
    fn baseline_counts_everything() {
        let evs: Vec<Event> = (0..5).map(|i| mark(i, i * 100)).collect();
        let r = summarize_delta(&evs, 0, "sid".into(), None, true);
        assert_eq!(r.prev_cursor, 0);
        assert_eq!(r.cursor, 5);
        assert_eq!(r.new_events, 5);
        assert_eq!(r.elapsed_ns, 400);
        assert_eq!(
            r.by_payload,
            vec![PayloadCount {
                kind: "Mark".into(),
                count: 5
            }]
        );
        assert_eq!(r.note, None);
    }

    #[test]
    fn delta_counts_only_after_cursor() {
        let evs: Vec<Event> = (0..5).map(|i| mark(i, i * 100)).collect();
        let r = summarize_delta(&evs, 3, "sid".into(), None, true);
        assert_eq!(r.prev_cursor, 3);
        assert_eq!(r.cursor, 5);
        assert_eq!(r.new_events, 2);
    }

    #[test]
    fn empty_delta_notes_no_new_events() {
        let evs: Vec<Event> = (0..5).map(|i| mark(i, i * 100)).collect();
        let r = summarize_delta(&evs, 5, "sid".into(), None, true);
        assert_eq!(r.new_events, 0);
        assert_eq!(r.note.as_deref(), Some("no new events"));
    }

    #[test]
    fn cursor_past_end_is_clamped_not_panic() {
        let evs: Vec<Event> = (0..3).map(|i| mark(i, i * 100)).collect();
        let r = summarize_delta(&evs, 99, "sid".into(), None, true);
        assert_eq!(r.new_events, 0);
        assert_eq!(r.cursor, 3);
        assert_eq!(r.note.as_deref(), Some("cursor ahead of log (clamped)"));
    }

    #[test]
    fn finalized_session_notes_finalized() {
        let evs: Vec<Event> = (0..5).map(|i| mark(i, i * 100)).collect();
        let r = summarize_delta(&evs, 3, "sid".into(), None, false);
        assert_eq!(r.note.as_deref(), Some("session finalized"));
    }
}

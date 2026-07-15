//! `smeltr tail`: stream the daemon event bus as NDJSON on stdout.

use anyhow::{Context, Result};
use smeltr_core::event::Event;
use std::collections::HashMap;
use std::io::Write;
use uuid::Uuid;

/// A detected gap in a session's seq stream (events dropped upstream).
#[derive(Debug, PartialEq)]
pub struct Gap {
    pub session_id: Uuid,
    pub expected: u64,
    pub got: u64,
}

/// Tracks per-session last seq to detect dropped events.
#[derive(Default)]
pub struct SeqTracker {
    last: HashMap<Uuid, u64>,
}

impl SeqTracker {
    /// Record `ev` and return `Some(Gap)` if its seq skipped past the next
    /// expected value for its session. The first event for a session sets a
    /// baseline (no gap) because `tail` joins the stream mid-flight.
    pub fn observe(&mut self, ev: &Event) -> Option<Gap> {
        match self.last.get(&ev.session_id).copied() {
            // First event for this session: baseline, no gap (tail joins mid-stream).
            None => {
                self.last.insert(ev.session_id, ev.seq);
                None
            }
            // Regression or duplicate (should not happen on the monotonic happy
            // path): ignore, and do not move the cursor backward.
            Some(prev) if ev.seq <= prev => None,
            // Forward jump: events were dropped upstream.
            Some(prev) if ev.seq > prev + 1 => {
                self.last.insert(ev.session_id, ev.seq);
                Some(Gap {
                    session_id: ev.session_id,
                    expected: prev + 1,
                    got: ev.seq,
                })
            }
            // Contiguous.
            Some(_) => {
                self.last.insert(ev.session_id, ev.seq);
                None
            }
        }
    }
}

pub fn to_ndjson(ev: &Event) -> String {
    serde_json::to_string(ev).unwrap_or_else(|_| "{}".to_string())
}

pub fn gap_line(gap: &Gap) -> String {
    format!(
        r#"{{"_smeltr_tail":"gap","session_id":"{}","expected":{},"got":{},"skipped":{}}}"#,
        gap.session_id,
        gap.expected,
        gap.got,
        gap.got - gap.expected
    )
}

pub fn passes(ev: &Event, want_session: Option<Uuid>) -> bool {
    match want_session {
        None => true,
        Some(id) => ev.session_id == id,
    }
}

/// Stream the daemon bus as NDJSON to stdout. `session` (optional) restricts to
/// one session (short id / UUID / name), resolved once at startup. Default: all
/// sessions (firehose). Live only — events from connection onward.
pub async fn run(session: Option<String>) -> Result<()> {
    let want: Option<Uuid> = match session {
        None => None,
        Some(ref s) => {
            let dir = smeltr_mcp::types::resolve_session(s)
                .map_err(|e| anyhow::anyhow!("{e}"))
                .with_context(|| format!("resolving session {s:?}"))?;
            let meta =
                smeltr_core::reader::read_metadata(&dir).context("reading session metadata")?;
            Some(meta.session_id.0)
        }
    };

    let sock = smeltr_daemon::server::socket_path();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(1024);
    let sock2 = sock.clone();
    let conn = tokio::spawn(async move {
        smeltr_daemon::client::subscribe_events(&sock2, "smeltr-tail", tx).await
    });

    let mut tracker = SeqTracker::default();
    while let Some(ev) = rx.recv().await {
        if !passes(&ev, want) {
            continue;
        }
        let gap = tracker.observe(&ev);
        let mut out = std::io::stdout().lock();
        if let Some(g) = gap {
            writeln!(out, "{}", gap_line(&g)).ok();
        }
        writeln!(out, "{}", to_ndjson(&ev)).ok();
        out.flush().ok();
    }

    // The stream ended: daemon shutdown/crash (EOF) or transport error.
    // Say so instead of exiting silently (#114).
    eprintln!("smeltr tail: daemon connection closed");
    match conn.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e)
            .context("daemon connection failed — is the daemon running? (`smeltr daemon start`)"),
        Err(e) => Err(anyhow::anyhow!("tail task failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Payload, Source};

    fn ev(session: Uuid, seq: u64) -> Event {
        Event {
            ts_mono_ns: seq,
            ts_wall_ns: 0,
            session_id: session,
            source: Source::Mark,
            pid: None,
            seq,
            payload: Payload::Mark {
                label: "m".into(),
                fields: Default::default(),
            },
        }
    }

    #[test]
    fn ndjson_round_trips_event() {
        let e = ev(Uuid::nil(), 7);
        let line = to_ndjson(&e);
        let back: Event = serde_json::from_str(&line).unwrap();
        assert_eq!(back.seq, 7);
        assert_eq!(back.session_id, Uuid::nil());
    }

    #[test]
    fn seqtracker_no_gap_on_contiguous() {
        let s = Uuid::from_u128(1);
        let mut t = SeqTracker::default();
        assert_eq!(t.observe(&ev(s, 5)), None); // baseline (mid-stream join)
        assert_eq!(t.observe(&ev(s, 6)), None);
        assert_eq!(t.observe(&ev(s, 7)), None);
    }

    #[test]
    fn seqtracker_detects_skip() {
        let s = Uuid::from_u128(1);
        let mut t = SeqTracker::default();
        assert_eq!(t.observe(&ev(s, 5)), None);
        assert_eq!(
            t.observe(&ev(s, 9)),
            Some(Gap {
                session_id: s,
                expected: 6,
                got: 9
            })
        );
        // after a gap, tracking resumes from the new value
        assert_eq!(t.observe(&ev(s, 10)), None);
    }

    #[test]
    fn seqtracker_is_per_session() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let mut t = SeqTracker::default();
        assert_eq!(t.observe(&ev(a, 5)), None);
        assert_eq!(t.observe(&ev(b, 100)), None); // different session: baseline, not a gap
        assert_eq!(t.observe(&ev(a, 6)), None); // a stays contiguous
        assert_eq!(t.observe(&ev(b, 101)), None);
    }

    #[test]
    fn gap_line_shape() {
        let g = Gap {
            session_id: Uuid::nil(),
            expected: 6,
            got: 9,
        };
        let line = gap_line(&g);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["_smeltr_tail"], "gap");
        assert_eq!(v["skipped"], 3);
        assert_eq!(v["expected"], 6);
        assert_eq!(v["got"], 9);
    }

    #[test]
    fn seqtracker_ignores_regression_without_false_gap() {
        let s = Uuid::from_u128(1);
        let mut t = SeqTracker::default();
        assert_eq!(t.observe(&ev(s, 5)), None); // baseline
        assert_eq!(t.observe(&ev(s, 4)), None); // regression/duplicate: no gap
        assert_eq!(t.observe(&ev(s, 6)), None); // next contiguous after 5 is still clean
    }

    #[test]
    fn passes_filters_by_session() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        assert!(passes(&ev(a, 0), None));
        assert!(passes(&ev(a, 0), Some(a)));
        assert!(!passes(&ev(a, 0), Some(b)));
    }
}

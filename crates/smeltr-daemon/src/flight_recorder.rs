//! In-memory ring of the most recent N seconds of events. Always active so
//! a sudden crash trigger can flush the recent history to a post-mortem
//! session on disk.

use smeltr_core::event::Event;
use std::collections::VecDeque;
use std::sync::Mutex;

pub struct FlightRecorder {
    window_ns: u64,
    inner: Mutex<VecDeque<Event>>,
}

impl FlightRecorder {
    pub fn new(window: std::time::Duration) -> Self {
        Self {
            window_ns: window.as_nanos() as u64,
            inner: Mutex::new(VecDeque::with_capacity(8192)),
        }
    }

    /// Push an event. Evicts events older than `newest.ts_mono_ns - window`.
    pub fn push(&self, ev: Event) {
        let mut q = self.inner.lock().unwrap();
        q.push_back(ev);
        let cutoff = q
            .back()
            .map(|e| e.ts_mono_ns)
            .unwrap_or(0)
            .saturating_sub(self.window_ns);
        while let Some(front) = q.front() {
            if front.ts_mono_ns < cutoff {
                q.pop_front();
            } else {
                break;
            }
        }
    }

    /// Returns a copy of the events currently in the ring.
    pub fn snapshot(&self) -> Vec<Event> {
        let q = self.inner.lock().unwrap();
        q.iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Event, Payload, Source};
    use uuid::Uuid;

    fn ev(ts: u64) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: ts,
            payload: Payload::Mark {
                label: format!("e-{ts}"),
                fields: Default::default(),
            },
        }
    }

    #[test]
    fn keeps_events_within_window() {
        let fr = FlightRecorder::new(std::time::Duration::from_secs(60));
        for i in 0..5 {
            fr.push(ev(i * 1_000_000_000));
        }
        assert_eq!(fr.len(), 5);
        let snap = fr.snapshot();
        assert_eq!(snap[0].ts_mono_ns, 0);
        assert_eq!(snap[4].ts_mono_ns, 4 * 1_000_000_000);
    }

    #[test]
    fn evicts_events_older_than_window() {
        let fr = FlightRecorder::new(std::time::Duration::from_secs(60));
        for i in 0..120 {
            fr.push(ev(i * 1_000_000_000));
        }
        let snap = fr.snapshot();
        assert!(!snap.is_empty());
        assert!(
            snap[0].ts_mono_ns >= 59 * 1_000_000_000,
            "front ts {} should be >= 59s",
            snap[0].ts_mono_ns
        );
        assert_eq!(snap.last().unwrap().ts_mono_ns, 119 * 1_000_000_000);
    }

    #[test]
    fn snapshot_does_not_drain() {
        let fr = FlightRecorder::new(std::time::Duration::from_secs(60));
        fr.push(ev(1));
        fr.push(ev(2));
        let _snap1 = fr.snapshot();
        let snap2 = fr.snapshot();
        assert_eq!(snap2.len(), 2);
    }
}

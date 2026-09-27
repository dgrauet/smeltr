//! In-memory ring of the most recent N seconds of events. Always active so
//! a sudden crash trigger can flush the recent history to a post-mortem
//! session on disk.

use smeltr_core::event::Event;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

/// Upper bound on buffered events, whatever the window (#267): a burst
/// inside 60 s grew the daemon to a 191 MB footprint.
pub const MAX_EVENTS: usize = 100_000;

pub struct FlightRecorder {
    window: std::time::Duration,
    max_events: usize,
    /// Events with the instant they were pushed. Eviction goes by that
    /// arrival time, not `ts_mono_ns`: each session stamps events from its
    /// own clock (the ambient one days old, a recording's near 0), so the
    /// stamps of two sessions are not comparable (#242).
    inner: Mutex<VecDeque<(Instant, Event)>>,
}

impl FlightRecorder {
    pub fn new(window: std::time::Duration) -> Self {
        Self::with_max_events(window, MAX_EVENTS)
    }

    pub fn with_max_events(window: std::time::Duration, max_events: usize) -> Self {
        Self {
            window,
            max_events,
            inner: Mutex::new(VecDeque::with_capacity(8192)),
        }
    }

    /// Push an event. Evicts events that arrived more than `window` ago, and
    /// the oldest beyond `max_events`.
    pub fn push(&self, ev: Event) {
        self.push_at(ev, Instant::now());
    }

    /// [`push`](Self::push) with an explicit arrival time, for tests.
    pub fn push_at(&self, ev: Event, now: Instant) {
        let mut q = self.inner.lock().unwrap();
        q.push_back((now, ev));
        while q.len() > self.max_events {
            q.pop_front();
        }
        while let Some((arrived, _)) = q.front() {
            if now.saturating_duration_since(*arrived) > self.window {
                q.pop_front();
            } else {
                break;
            }
        }
    }

    /// Returns a copy of the events currently in the ring, oldest first.
    pub fn snapshot(&self) -> Vec<Event> {
        let q = self.inner.lock().unwrap();
        q.iter().map(|(_, e)| e.clone()).collect()
    }

    /// Panic-safe snapshot: never blocks, never panics. Returns `None` only
    /// when another thread holds the lock; a poisoned lock is recovered.
    pub fn try_snapshot(&self) -> Option<Vec<Event>> {
        crate::sync_util::try_lock_recover(&self.inner)
            .map(|q| q.iter().map(|(_, e)| e.clone()).collect())
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

    /// #267: bounded by time only, a burst (100k × 1 KB events inside the
    /// window) peaked the daemon at a 191 MB footprint. The newest events
    /// are the ones a post-mortem needs.
    #[test]
    fn keeps_at_most_max_events_newest_first() {
        let fr = FlightRecorder::with_max_events(std::time::Duration::from_secs(60), 10);
        let t0 = Instant::now();
        for i in 0..25 {
            fr.push_at(ev(i), t0);
        }
        let kept: Vec<u64> = fr.snapshot().iter().map(|e| e.seq).collect();
        assert_eq!(kept, (15..25).collect::<Vec<_>>());
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
        let t0 = std::time::Instant::now();
        for i in 0..120 {
            fr.push_at(ev(i), t0 + std::time::Duration::from_secs(i));
        }
        let snap = fr.snapshot();
        assert_eq!(snap.first().unwrap().seq, 59, "arrived more than 60 s ago");
        assert_eq!(snap.last().unwrap().seq, 119);
    }

    /// #242: every session stamps `ts_mono_ns` from its own clock — the
    /// ambient session's is days old, a recording's starts near 0. Evicting
    /// on it threw a recording's events out as soon as an ambient event
    /// arrived, so a post-mortem missed the very run that crashed.
    #[test]
    fn a_recordings_events_survive_an_ambient_event() {
        let fr = FlightRecorder::new(std::time::Duration::from_secs(60));
        let days = 3 * 24 * 3600 * 1_000_000_000u64;
        fr.push(ev(1_000_000_000)); // recording, 1 s after it started
        fr.push(ev(days)); // ambient, days after the daemon started
        assert_eq!(fr.len(), 2);
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

    #[test]
    fn try_snapshot_returns_events_when_uncontended() {
        let fr = FlightRecorder::new(std::time::Duration::from_secs(60));
        fr.push(ev(1));
        assert_eq!(fr.try_snapshot().unwrap().len(), 1);
    }

    #[test]
    fn try_snapshot_recovers_from_poisoned_lock() {
        let fr = std::sync::Arc::new(FlightRecorder::new(std::time::Duration::from_secs(60)));
        fr.push(ev(1));
        let fr2 = fr.clone();
        // Poison the mutex: panic while holding the guard.
        let _ = std::thread::spawn(move || {
            let _guard = fr2.inner.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert_eq!(
            fr.try_snapshot().unwrap().len(),
            1,
            "poisoned lock must be recovered"
        );
    }

    #[test]
    fn try_snapshot_returns_none_when_lock_held() {
        let fr = std::sync::Arc::new(FlightRecorder::new(std::time::Duration::from_secs(60)));
        let guard = fr.inner.lock().unwrap();
        assert!(fr.try_snapshot().is_none());
        drop(guard);
        assert!(fr.try_snapshot().is_some());
    }
}

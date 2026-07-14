//! Pure replay-timeline state: a cursor over the in-memory event list driven
//! by a virtual clock (wall dt × speed). Seeking moves the clock; backward
//! seeks tell the caller to rebuild UiState from scratch (Rewind).

use smeltr_core::event::Event;
use std::ops::Range;
use std::time::Duration;

#[derive(Debug)]
pub struct ScrubState {
    events: Vec<Event>,
    base_ns: u64,
    cursor: usize,
    virtual_ns: u64,
    speed: f64,
}

#[derive(Debug)]
pub enum SeekOutcome {
    /// Ingest exactly this slice of events.
    Forward(Range<usize>),
    /// Rebuild UiState from scratch by folding this slice (always starts at 0).
    Rewind(Range<usize>),
}

impl ScrubState {
    pub fn new(mut events: Vec<Event>, speed: f64) -> Self {
        if !events.is_sorted_by_key(|e| e.ts_mono_ns) {
            events.sort_by_key(|e| e.ts_mono_ns);
        }
        let base_ns = events.first().map(|e| e.ts_mono_ns).unwrap_or(0);
        let mut s = Self {
            events,
            base_ns,
            cursor: 0,
            virtual_ns: 0,
            speed,
        };
        if s.speed <= 0.0 {
            // Historical `--speed 0` = as fast as possible: start fully played.
            s.virtual_ns = s.duration_ns();
            s.cursor = s.events.len();
        }
        s
    }

    fn rel_ts(&self, i: usize) -> u64 {
        self.events[i].ts_mono_ns.saturating_sub(self.base_ns)
    }

    /// Move the cursor forward past every event with rel ts <= virtual_ns.
    fn catch_up(&mut self) -> Range<usize> {
        let start = self.cursor;
        while self.cursor < self.events.len() && self.rel_ts(self.cursor) <= self.virtual_ns {
            self.cursor += 1;
        }
        start..self.cursor
    }

    pub fn advance(&mut self, wall_dt: Duration) -> Range<usize> {
        if self.speed <= 0.0 || self.at_end() {
            return self.cursor..self.cursor;
        }
        let dt_ns = (wall_dt.as_nanos() as f64 * self.speed) as u64;
        self.virtual_ns = (self.virtual_ns + dt_ns).min(self.duration_ns());
        self.catch_up()
    }

    /// Moves the clock forward to `target` and returns the crossed slice.
    fn forward_to(&mut self, target: u64) -> Range<usize> {
        self.virtual_ns = target;
        self.catch_up()
    }

    /// Moves the clock backward to `target`; the returned range always
    /// starts at 0 (the caller rebuilds state from scratch).
    fn rewind_to(&mut self, target: u64) -> Range<usize> {
        self.virtual_ns = target;
        self.cursor = 0;
        self.catch_up()
    }

    pub fn seek_by_secs(&mut self, delta: i64) -> SeekOutcome {
        let target = if delta >= 0 {
            self.virtual_ns.saturating_add(delta as u64 * 1_000_000_000)
        } else {
            self.virtual_ns
                .saturating_sub(delta.unsigned_abs() * 1_000_000_000)
        };
        let target = target.min(self.duration_ns());
        if delta < 0 {
            // Backward seeks always rebuild from scratch.
            let r = if target < self.virtual_ns {
                self.rewind_to(target)
            } else {
                // Clamped to the same position (already at 0): rebuild 0..cursor,
                // reproducing the current state exactly rather than wiping it.
                0..self.cursor
            };
            SeekOutcome::Rewind(r)
        } else {
            SeekOutcome::Forward(self.forward_to(target))
        }
    }

    pub fn seek_to_ns(&mut self, ns: u64) -> SeekOutcome {
        let target = ns.min(self.duration_ns());
        if target >= self.virtual_ns {
            SeekOutcome::Forward(self.forward_to(target))
        } else {
            SeekOutcome::Rewind(self.rewind_to(target))
        }
    }

    pub fn position_ns(&self) -> u64 {
        self.virtual_ns
    }

    pub fn duration_ns(&self) -> u64 {
        match (self.events.first(), self.events.last()) {
            (Some(f), Some(l)) => l.ts_mono_ns.saturating_sub(f.ts_mono_ns),
            _ => 0,
        }
    }

    #[cfg(test)]
    fn progress(&self) -> f64 {
        let d = self.duration_ns();
        if d == 0 {
            return 0.0;
        }
        self.virtual_ns as f64 / d as f64
    }

    pub fn at_end(&self) -> bool {
        self.cursor >= self.events.len() && self.virtual_ns >= self.duration_ns()
    }

    pub fn events(&self) -> &[Event] {
        &self.events
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
                label: format!("e{ts}"),
                fields: Default::default(),
            },
        }
    }

    const S: u64 = 1_000_000_000;

    fn state() -> ScrubState {
        // events at 0s, 1s, 2s, 10s (base offset exercises relativity)
        ScrubState::new(vec![ev(5 * S), ev(6 * S), ev(7 * S), ev(15 * S)], 1.0)
    }

    #[test]
    fn advance_crosses_events_at_virtual_times() {
        let mut s = state();
        assert_eq!(s.advance(std::time::Duration::from_millis(500)), 0..1); // t=0.5s → event@0
        assert_eq!(s.advance(std::time::Duration::from_secs(1)), 1..2); // t=1.5s → event@1s
        assert_eq!(s.advance(std::time::Duration::from_secs(1)), 2..3); // t=2.5s → event@2s
        assert_eq!(s.advance(std::time::Duration::from_secs(1)), 3..3); // t=3.5s → nothing
    }

    #[test]
    fn advance_respects_speed() {
        let mut s = ScrubState::new(vec![ev(0), ev(10 * S)], 10.0);
        assert_eq!(s.advance(std::time::Duration::from_secs(1)), 0..2); // 1s wall × 10 = 10s
    }

    #[test]
    fn speed_zero_starts_at_end() {
        let s = ScrubState::new(vec![ev(0), ev(S)], 0.0);
        assert!(s.at_end());
        assert_eq!(s.position_ns(), s.duration_ns());
    }

    #[test]
    fn seek_forward_returns_skipped_slice() {
        let mut s = state();
        match s.seek_by_secs(5) {
            SeekOutcome::Forward(r) => assert_eq!(r, 0..3), // 0,1,2s crossed; 10s not
            other => panic!("expected Forward, got {other:?}"),
        }
        assert_eq!(s.position_ns(), 5 * S);
    }

    #[test]
    fn seek_backward_returns_rewind_from_zero() {
        let mut s = state();
        let _ = s.seek_by_secs(30); // clamp to end (10s), all 4 ingested
        assert!(s.at_end());
        match s.seek_by_secs(-5) {
            SeekOutcome::Rewind(r) => assert_eq!(r, 0..3), // back to t=5s → events 0..3
            other => panic!("expected Rewind, got {other:?}"),
        }
        assert_eq!(s.position_ns(), 5 * S);
    }

    #[test]
    fn seeks_clamp_at_both_ends() {
        let mut s = state();
        match s.seek_by_secs(-5) {
            SeekOutcome::Rewind(r) => assert_eq!(r, 0..0), // already at 0, clamped
            other => panic!("expected Rewind, got {other:?}"),
        }
        assert_eq!(s.position_ns(), 0);
        let _ = s.seek_by_secs(999);
        assert_eq!(s.position_ns(), 10 * S); // clamped to duration
        assert!(s.at_end());
    }

    #[test]
    fn progress_and_empty_session() {
        let mut e = ScrubState::new(Vec::new(), 1.0);
        assert_eq!(e.duration_ns(), 0);
        assert_eq!(e.progress(), 0.0);
        assert_eq!(e.advance(std::time::Duration::from_secs(1)), 0..0);
        assert!(matches!(e.seek_by_secs(5), SeekOutcome::Forward(r) if r == (0..0)));

        let mut s = state();
        let _ = s.seek_by_secs(5);
        assert!((s.progress() - 0.5).abs() < 1e-9);
    }

    #[test]
    fn unsorted_events_are_sorted_on_construction() {
        let s = ScrubState::new(vec![ev(2 * S), ev(0)], 1.0);
        assert_eq!(s.events()[0].ts_mono_ns, 0);
        assert_eq!(s.duration_ns(), 2 * S);
    }

    #[test]
    fn repeated_seek_back_at_start_does_not_replay_first_event() {
        let mut s = ScrubState::new(vec![ev(0), ev(S)], 1.0);
        let _ = s.seek_to_ns(0); // Forward: ingests event@0, cursor=1
        let _ = s.seek_by_secs(-1); // clamped no-op at start
        assert_eq!(
            s.advance(std::time::Duration::ZERO),
            1..1,
            "event 0 must not be re-emitted after a clamped seek-back"
        );
    }

    #[test]
    fn clamped_seek_back_after_ingest_rewinds_full_prefix_not_empty() {
        let mut s = ScrubState::new(vec![ev(0), ev(S)], 1.0);
        let _ = s.seek_to_ns(0); // Forward: ingests event@0, cursor=1
        match s.seek_by_secs(-5) {
            // clamped no-op at start (already at 0)
            SeekOutcome::Rewind(r) => assert_eq!(
                r,
                0..1,
                "clamped seek-back must rebuild the full 0..cursor prefix, not an empty range"
            ),
            other => panic!("expected Rewind, got {other:?}"),
        }
    }
}

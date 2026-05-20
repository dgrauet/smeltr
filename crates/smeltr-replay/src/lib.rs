//! Session replay: read events from disk and yield them to a channel with
//! original-spaced timing scaled by a `speed` factor.

use smeltr_core::event::Event;
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct Replayer {
    events: Vec<Event>,
}

impl Replayer {
    pub fn from_dir(dir: &Path) -> std::io::Result<Self> {
        let events = smeltr_core::reader::read_events(dir)?;
        Ok(Self { events })
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn duration(&self) -> Duration {
        if self.events.len() < 2 {
            return Duration::ZERO;
        }
        let first = self.events.first().unwrap().ts_mono_ns;
        let last = self.events.last().unwrap().ts_mono_ns;
        Duration::from_nanos(last.saturating_sub(first))
    }

    /// Streams events to `tx` at original-spaced timing scaled by `speed`.
    /// `speed = 1.0` = real time, `speed = 10.0` = 10× faster.
    /// `speed = 0.0` means no delay (as fast as the receiver consumes).
    pub async fn play(&self, speed: f64, tx: mpsc::Sender<Event>) {
        if self.events.is_empty() {
            return;
        }
        let first_ts = self.events.first().unwrap().ts_mono_ns;
        let start = tokio::time::Instant::now();
        for ev in &self.events {
            if speed > 0.0 {
                let elapsed_in_session = ev.ts_mono_ns.saturating_sub(first_ts);
                let target_offset_ns = (elapsed_in_session as f64 / speed) as u64;
                let target = start + Duration::from_nanos(target_offset_ns);
                tokio::time::sleep_until(target).await;
            }
            if tx.send(ev.clone()).await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    fn temp_session_with(events: &[Event]) -> (tempfile::TempDir, std::path::PathBuf) {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let meta = SessionMetadata::now_starting(SessionId::new());
        let mut w = SessionWriter::create(meta).unwrap();
        for ev in events {
            w.write_event(ev).unwrap();
        }
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "2026-05-14T00:00:00Z".into()).unwrap();
        (home, dir)
    }

    fn mk_event(ts: u64, label: &str) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: ts,
            payload: Payload::Mark {
                label: label.into(),
                fields: Default::default(),
            },
        }
    }

    #[test]
    #[serial_test::serial]
    fn from_dir_reads_events() {
        let evs = vec![mk_event(0, "a"), mk_event(100, "b")];
        let (_home, dir) = temp_session_with(&evs);
        let r = Replayer::from_dir(&dir).unwrap();
        assert_eq!(r.events().len(), 2);
    }

    #[test]
    #[serial_test::serial]
    fn duration_matches_first_to_last_delta() {
        let evs = vec![mk_event(0, "a"), mk_event(1_000_000_000, "b")];
        let (_home, dir) = temp_session_with(&evs);
        let r = Replayer::from_dir(&dir).unwrap();
        assert_eq!(r.duration(), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn play_at_high_speed_yields_all_events_quickly() {
        let evs = vec![
            mk_event(0, "a"),
            mk_event(1_000_000_000, "b"),
            mk_event(2_000_000_000, "c"),
        ];
        let (_home, dir) = temp_session_with(&evs);
        let r = Replayer::from_dir(&dir).unwrap();
        let (tx, mut rx) = mpsc::channel::<Event>(16);
        let play_task = tokio::spawn(async move { r.play(1000.0, tx).await });
        let mut got = Vec::new();
        while let Some(ev) = rx.recv().await {
            got.push(ev);
        }
        play_task.await.unwrap();
        assert_eq!(got.len(), 3);
        assert!(matches!(got[0].payload, Payload::Mark { ref label, .. } if label == "a"));
    }

    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn play_speed_zero_skips_delay() {
        let evs = vec![mk_event(0, "a"), mk_event(60_000_000_000, "b")];
        let (_home, dir) = temp_session_with(&evs);
        let r = Replayer::from_dir(&dir).unwrap();
        let (tx, mut rx) = mpsc::channel::<Event>(4);
        let h = tokio::spawn(async move { r.play(0.0, tx).await });
        let _ = rx.recv().await;
        let _ = rx.recv().await;
        h.await.unwrap();
    }
}

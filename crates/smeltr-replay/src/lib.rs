//! Session replay: read a session's events from disk. The TUI drives the
//! timing itself (`smeltr_tui::scrub`), with seek and pause.

use smeltr_core::event::Event;
use std::path::Path;

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
}

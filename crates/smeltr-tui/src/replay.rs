//! Replay mode adapter: load a session from disk into a ScrubState the TUI
//! drives with its virtual clock (seek/pause), replacing the old
//! fire-and-forget channel stream.

use crate::scrub::ScrubState;
use smeltr_replay::Replayer;
use std::path::Path;

pub fn load(dir: &Path, speed: f64) -> std::io::Result<ScrubState> {
    let r = Replayer::from_dir(dir)?;
    Ok(ScrubState::new(r.events().to_vec(), speed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Event, Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

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

    fn temp_session_with_two_events() -> (tempfile::TempDir, std::path::PathBuf) {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let meta = SessionMetadata::now_starting(SessionId::new());
        let mut w = SessionWriter::create(meta).unwrap();
        for ev in [mk_event(0, "a"), mk_event(100, "b")] {
            w.write_event(&ev).unwrap();
        }
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "2026-05-14T00:00:00Z".into()).unwrap();
        (home, dir)
    }

    #[test]
    #[serial_test::serial]
    fn load_builds_scrub_state_from_session_dir() {
        let (_home, dir) = temp_session_with_two_events();
        let s = load(&dir, 1.0).unwrap();
        assert_eq!(s.events().len(), 2);
        assert!(!s.at_end());
    }
}

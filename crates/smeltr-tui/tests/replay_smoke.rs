//! End-to-end: session on disk -> replay::load -> ScrubState -> UiState.

use smeltr_core::event::{Event, Payload, Source};
use smeltr_core::session::{SessionId, SessionMetadata};
use smeltr_core::writer::SessionWriter;
use smeltr_tui::UiState;
use uuid::Uuid;

#[test]
#[serial_test::serial]
fn replay_drains_session_into_state() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("SMELTR_HOME", home.path());

    let id = SessionId::new();
    let meta = SessionMetadata::now_starting(id);
    let mut w = SessionWriter::create(meta).unwrap();
    for i in 0..5 {
        w.write_event(&Event {
            ts_mono_ns: i,
            ts_wall_ns: i,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: i,
            payload: Payload::Mark {
                label: format!("r-{i}"),
                fields: Default::default(),
            },
        })
        .unwrap();
    }
    let dir = w.dir().to_path_buf();
    w.finalize(Some(0), "2026-05-14T00:00:00Z".into()).unwrap();

    // speed = 0.0 starts the ScrubState fully played (historical "as fast as possible").
    let scrub = smeltr_tui::replay::load(&dir, 0.0).unwrap();
    assert!(scrub.at_end());
    let state = UiState::rebuild(scrub.events());
    // SessionStarted (auto-emitted) + 5 Marks = 6 events.
    assert!(
        state.events_total >= 5,
        "expected >= 5 events, got {}",
        state.events_total
    );
    assert!(
        state
            .mlx_recent_marks
            .iter()
            .any(|(_, l)| l.starts_with("r-")),
        "expected r-* marks in state"
    );
}

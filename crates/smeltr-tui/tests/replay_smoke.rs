//! End-to-end: session on disk -> replay -> UiState.

use smeltr_core::event::{Event, Payload, Source};
use smeltr_core::session::{SessionId, SessionMetadata};
use smeltr_core::writer::SessionWriter;
use smeltr_tui::UiState;
use tokio::sync::mpsc;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn replay_drains_session_into_state() {
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
            },
        })
        .unwrap();
    }
    let dir = w.dir().to_path_buf();
    w.finalize(Some(0), "2026-05-14T00:00:00Z".into()).unwrap();

    let (tx, mut rx) = mpsc::channel(64);
    let h = tokio::spawn(async move { smeltr_tui::replay::spawn(&dir, 0.0, tx).await });
    let mut state = UiState::default();
    while let Some(ev) = rx.recv().await {
        state.ingest(&ev);
    }
    h.await.unwrap().unwrap();
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

//! Fixtures shared by the tools' unit tests.

use smeltr_core::event::{Event, Payload, Source};
use smeltr_core::session::{SessionId, SessionMetadata};
use smeltr_core::writer::SessionWriter;

/// A finished session in which op-timing sampling auto-disabled twice
/// (#165). Returns its reference. `SMELTR_HOME` must already be set.
pub fn sampling_disabled_session() -> String {
    let id = SessionId::new();
    let mut w = SessionWriter::create(SessionMetadata::now_starting(id)).unwrap();
    for (seq, reason) in [
        (
            1u64,
            "stage sampling disabled after sustained alloc failures",
        ),
        (
            2,
            "dispatch sampling disabled after sustained alloc failures",
        ),
    ] {
        w.write_event(&Event {
            ts_mono_ns: seq,
            ts_wall_ns: seq,
            session_id: uuid::Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq,
            payload: Payload::MetalHookSkipped {
                reason: reason.into(),
            },
        })
        .unwrap();
    }
    w.finalize(Some(0), "ok".into()).unwrap();
    id.short()
}

/// Asserts `notice` is the shared #165 wording for two episodes.
pub fn assert_degraded(notice: Option<String>) {
    let notice = notice.expect("degraded notice");
    assert!(notice.contains("2 time(s)"), "{notice}");
    assert!(notice.contains("partial"), "{notice}");
}

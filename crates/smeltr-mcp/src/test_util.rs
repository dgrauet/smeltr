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

/// A finished session whose only command buffer ran a GEMM and an add in
/// one encoder, so the hook split its time between them by dispatch count
/// (#314). Returns its reference. `SMELTR_HOME` must already be set.
pub fn apportioned_session() -> String {
    use smeltr_core::event::OpSample;
    let id = SessionId::new();
    let mut w = SessionWriter::create(SessionMetadata::now_starting(id)).unwrap();
    let ev = |seq: u64, payload: Payload| Event {
        ts_mono_ns: seq * 10,
        ts_wall_ns: seq * 10,
        session_id: uuid::Uuid::nil(),
        source: Source::MetalHook,
        pid: None,
        seq,
        payload,
    };
    let op = |name: &str| OpSample {
        name: name.into(),
        symbol: Some(name.into()),
        gpu_ns: 500,
        count: 1,
        apportioned_ns: Some(500),
    };
    for e in [
        ev(
            1,
            Payload::MetalCbCommitted {
                cb_id: 1,
                queue_id: 1,
                queue_depth: 1,
                label: None,
            },
        ),
        ev(
            2,
            Payload::MetalCbCompleted {
                cb_id: 1,
                queue_id: 1,
                status: 4,
                error_code: None,
                error_domain: None,
                in_flight_ns: 1_000_000,
            },
        ),
        ev(
            3,
            Payload::MetalCbOps {
                cb_id: 1,
                ops: vec![op("steel_gemm_fused_nn_bfloat16"), op("vv_Addbfloat16")],
            },
        ),
    ] {
        w.write_event(&e).unwrap();
    }
    w.finalize(Some(0), "ok".into()).unwrap();
    id.short()
}

/// Asserts `notice` is the shared #314 wording for a fully apportioned run.
pub fn assert_apportioned(notice: Option<String>) {
    let notice = notice.expect("op times estimated notice");
    assert!(notice.contains("100%"), "{notice}");
    assert!(notice.contains("--exact-ops"), "{notice}");
}

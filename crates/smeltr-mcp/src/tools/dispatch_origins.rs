//! `get_dispatch_origins` MCP tool: per-(kind, file:line) GPU time attribution.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_analyzer::dispatch_origins::{compute_dispatch_origins, DispatchOrigin};
use smeltr_core::reader::read_events;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    pub session: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub origins: Vec<DispatchOrigin>,
    /// Why `origins` is empty, when it is — same hint as `smeltr origins`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Set when op-timing sampling auto-disabled during the run (#165):
    /// the per-op `gpu_ns` below are incomplete over those spans. Same
    /// wording as the CLI and `get_inference_breakdown`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub degraded: Option<String>,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let dir = resolve_session(&params.session)?;
    let events = read_events(&dir)?;
    let origins = compute_dispatch_origins(&events);
    let note = origins.is_empty().then(|| {
        "no dispatch origins — was the session recorded with SMELTR_STACK_CAPTURE=1?".to_string()
    });
    Ok(Response {
        origins,
        note,
        degraded: smeltr_analyzer::degraded_advice(
            smeltr_analyzer::diff::sampling_disable_episodes(&events),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Event, OpSample, Payload, Source, StackFrame};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    fn ev(seq: u64, ts: u64, source: Source, payload: Payload) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source,
            pid: None,
            seq,
            payload,
        }
    }

    #[test]
    #[serial_test::serial]
    fn dispatch_origins_returns_origins() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        let evs = vec![
            ev(
                1,
                10,
                Source::PythonSidecar,
                Payload::MlxEvalEntered {
                    call_id: 1,
                    array_count: 1,
                    stream: "gpu".into(),
                    module_stack: vec![],
                    stack_frames: vec![StackFrame {
                        filename: "/work/attention.py".into(),
                        lineno: 127,
                        funcname: "forward".into(),
                    }],
                },
            ),
            // The lifecycle the hook emits before the ops: origins keys on
            // the commit (#243).
            ev(
                100,
                14,
                Source::MetalHook,
                Payload::MetalCbCommitted {
                    cb_id: 9,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
            ),
            ev(
                101,
                15,
                Source::MetalHook,
                Payload::MetalCbCompleted {
                    cb_id: 9,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 1,
                },
            ),
            ev(
                2,
                15,
                Source::MetalHook,
                Payload::MetalCbOps {
                    cb_id: 9,
                    ops: vec![OpSample {
                        name: "K_x".into(),
                        symbol: Some("gemm_bf16".into()),
                        gpu_ns: 1_000_000,
                        count: 5,
                    }],
                },
            ),
            ev(
                3,
                20,
                Source::PythonSidecar,
                Payload::MlxEvalReturned {
                    call_id: 1,
                    duration_ns: 10,
                    was_async: false,
                },
            ),
        ];
        for e in &evs {
            w.write_event(e).unwrap();
        }
        w.finalize(Some(0), "x".into()).unwrap();

        let resp = run(Params {
            session: id.short(),
        })
        .unwrap();
        assert_eq!(resp.origins.len(), 1);
        assert_eq!(resp.origins[0].kind, "Matmul");
        assert_eq!(resp.origins[0].file_line, "attention.py:127");
        assert_eq!(resp.origins[0].gpu_ns, 1_000_000);
        assert_eq!(resp.origins[0].dispatch_count, 5);
    }

    /// #165 parity (#243): `smeltr origins` warns; the tool did not.
    #[test]
    #[serial_test::serial]
    fn sampling_disabled_session_surfaces_degraded_notice() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let session = crate::test_util::sampling_disabled_session();
        let resp = run(Params { session }).unwrap();
        crate::test_util::assert_degraded(resp.degraded);
    }

    /// #243: `smeltr origins` explains an empty result (no stack capture);
    /// the tool returned a bare empty list.
    #[test]
    #[serial_test::serial]
    fn empty_origins_carry_the_capture_hint() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let session = crate::test_util::sampling_disabled_session();
        let resp = run(Params { session }).unwrap();
        assert!(resp.origins.is_empty());
        let note = resp.note.expect("note");
        assert!(note.contains("SMELTR_STACK_CAPTURE=1"), "{note}");
    }
}

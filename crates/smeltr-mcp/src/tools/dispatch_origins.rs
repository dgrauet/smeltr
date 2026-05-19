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
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let dir = resolve_session(&params.session)?;
    let events = read_events(&dir)?;
    Ok(Response {
        origins: compute_dispatch_origins(&events),
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
}

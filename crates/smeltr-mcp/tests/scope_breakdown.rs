//! Verifies that a synthesized session with a user `Scope` frame produces
//! a breakdown tree containing the user's chosen qualname.

use smeltr_core::event::{Event, Payload, Source};
use smeltr_core::session::{SessionId, SessionMetadata};
use smeltr_core::writer::SessionWriter;
use smeltr_mcp::tools::inference_breakdown::{run, Params};
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
fn user_scope_appears_as_qualname_in_breakdown_tree() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("SMELTR_HOME", home.path());
    let id = SessionId::new();
    let meta = SessionMetadata::now_starting(id);
    let mut w = SessionWriter::create(meta).unwrap();

    let evs = vec![
        ev(
            1,
            1,
            Source::PythonSidecar,
            Payload::ModuleEntered {
                module_call_id: 1,
                module_def_id: 1,
                qualname: "denoise.pass:cond".into(),
                class_name: "Scope".into(),
                parent_call_id: None,
                depth: 0,
                fields: Default::default(),
            },
        ),
        ev(
            2,
            10,
            Source::PythonSidecar,
            Payload::MlxEvalEntered {
                call_id: 1,
                array_count: 1,
                stream: "gpu".into(),
                module_stack: vec![1],
                stack_frames: vec![],
            },
        ),
        ev(
            3,
            20,
            Source::MetalHook,
            Payload::MetalCbCommitted {
                cb_id: 9,
                queue_id: 1,
                queue_depth: 1,
                label: None,
            },
        ),
        ev(
            4,
            30,
            Source::MetalHook,
            Payload::MetalCbCompleted {
                cb_id: 9,
                queue_id: 1,
                status: 4,
                error_code: None,
                error_domain: None,
                in_flight_ns: 1_000_000,
            },
        ),
        ev(
            5,
            40,
            Source::PythonSidecar,
            Payload::MlxEvalReturned {
                call_id: 1,
                duration_ns: 30,
                was_async: false,
            },
        ),
        ev(
            6,
            50,
            Source::PythonSidecar,
            Payload::ModuleReturned { module_call_id: 1 },
        ),
    ];
    for e in &evs {
        w.write_event(e).unwrap();
    }
    w.finalize(Some(0), "x".into()).unwrap();

    let resp = run(Params {
        session: id.short(),
        ..Default::default()
    })
    .unwrap();

    let scope_node = resp
        .root
        .children
        .iter()
        .find(|c| c.qualname == "denoise.pass:cond")
        .expect("expected user scope qualname in the breakdown tree");
    assert!(scope_node.gpu_ns_subtree > 0);
}

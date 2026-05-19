use assert_cmd::Command;
use smeltr_core::event::{Event, Payload, Source};
use smeltr_core::session::{SessionId, SessionMetadata};
use smeltr_core::writer::SessionWriter;
use uuid::Uuid;

#[test]
#[serial_test::serial]
fn breakdown_command_renders_table() {
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("SMELTR_HOME", home.path());
    let id = SessionId::new();
    let meta = SessionMetadata::now_starting(id);
    let mut w = SessionWriter::create(meta).unwrap();
    let evs: Vec<Event> = vec![
        Event {
            ts_mono_ns: 10,
            ts_wall_ns: 10,
            session_id: Uuid::nil(),
            source: Source::PythonSidecar,
            pid: None,
            seq: 1,
            payload: Payload::ModuleEntered {
                module_call_id: 1,
                module_def_id: 1,
                qualname: "Linear".into(),
                class_name: "Linear".into(),
                parent_call_id: None,
                depth: 0,
            },
        },
        Event {
            ts_mono_ns: 100,
            ts_wall_ns: 100,
            session_id: Uuid::nil(),
            source: Source::PythonSidecar,
            pid: None,
            seq: 2,
            payload: Payload::MlxEvalEntered {
                call_id: 7,
                array_count: 1,
                stream: "gpu".into(),
                module_stack: vec![1],
                stack_frames: vec![],
            },
        },
        Event {
            ts_mono_ns: 110,
            ts_wall_ns: 110,
            session_id: Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq: 3,
            payload: Payload::MetalCbCommitted {
                cb_id: 9,
                queue_id: 1,
                queue_depth: 1,
                label: None,
            },
        },
        Event {
            ts_mono_ns: 120,
            ts_wall_ns: 120,
            session_id: Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq: 4,
            payload: Payload::MetalCbCompleted {
                cb_id: 9,
                queue_id: 1,
                status: 4,
                error_code: None,
                error_domain: None,
                in_flight_ns: 700,
            },
        },
        Event {
            ts_mono_ns: 200,
            ts_wall_ns: 200,
            session_id: Uuid::nil(),
            source: Source::PythonSidecar,
            pid: None,
            seq: 5,
            payload: Payload::MlxEvalReturned {
                call_id: 7,
                duration_ns: 100,
                was_async: false,
            },
        },
        Event {
            ts_mono_ns: 210,
            ts_wall_ns: 210,
            session_id: Uuid::nil(),
            source: Source::PythonSidecar,
            pid: None,
            seq: 6,
            payload: Payload::ModuleReturned { module_call_id: 1 },
        },
    ];
    for e in &evs {
        w.write_event(e).unwrap();
    }
    w.finalize(Some(0), "2026-05-15T00:00:00Z".into()).unwrap();

    let mut cmd = Command::cargo_bin("smeltr").unwrap();
    cmd.env("SMELTR_HOME", home.path())
        .args(["breakdown", &id.short()]);
    let out = cmd.assert().success();
    let stdout = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    assert!(stdout.contains("Linear"), "stdout was: {stdout}");
    assert!(stdout.contains("0.700"));
}

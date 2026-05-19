//! End-to-end repro for #31 Gap 1: when smeltr record's child is a launcher
//! (here, /bin/sh) that emits a Mark from a different PID via the daemon
//! socket carrying SMELTR_SCOPE_TOKEN, the Mark must land in the scoped
//! session, NOT ambient.
//!
//! This test exercises the router's token-first path directly via the
//! daemon protocol (no Python required), simulating what the Python
//! sidecar will do once it reads SMELTR_SCOPE_TOKEN.

use serial_test::serial;
use smeltr_core::event::{Payload, Source};
use smeltr_daemon::session_router::SessionRouter;
use smeltr_daemon::sessions::ActiveSession;
use std::sync::Arc;

fn temp_home() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::env::set_var("SMELTR_HOME", d.path());
    d
}

#[test]
#[serial]
fn grandchild_emit_with_token_lands_in_scoped_session() {
    let _h = temp_home();
    let ambient = Arc::new(ActiveSession::open_new().unwrap());
    let router = SessionRouter::new(ambient.clone(), None, None);

    // Parent process: smeltr record spawns child PID 1000 (the launcher).
    let scoped_id = router
        .attach_scoped(
            1000,
            vec!["sh".into(), "-c".into()],
            Some("UUID-X".into()),
            None,
        )
        .unwrap();

    // Grandchild (e.g. python via uv) has a different PID but inherits the
    // env var. Simulate the emit it would send via the Unix socket.
    router
        .append(
            Source::PythonSidecar,
            Some(31337),
            Some("UUID-X"),
            Payload::Mark {
                label: "from-grandchild-with-token".into(),
            },
        )
        .unwrap();

    router.detach_scoped(1000, Some(0));
    ambient.finalize(Some(0), "test").unwrap();

    let dirs = smeltr_core::reader::list_sessions().unwrap();
    let mut found_in_scoped = false;
    let mut found_in_ambient = false;
    for d in &dirs {
        let meta = smeltr_core::reader::read_metadata(d).unwrap();
        let evs = smeltr_core::reader::read_events(d).unwrap();
        let has = evs.iter().any(|e| {
            matches!(
                &e.payload,
                Payload::Mark { label } if label == "from-grandchild-with-token"
            )
        });
        if meta.session_id == scoped_id && has {
            found_in_scoped = true;
        }
        if matches!(meta.kind, smeltr_core::session::SessionKind::Ambient) && has {
            found_in_ambient = true;
        }
    }
    assert!(
        found_in_scoped,
        "the token-tagged Mark must land in the scoped session"
    );
    assert!(
        !found_in_ambient,
        "ambient must not see the token-tagged Mark"
    );
}

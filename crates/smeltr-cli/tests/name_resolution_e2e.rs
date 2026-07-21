//! End-to-end repro for #31 Gap 3: when smeltr record sends a session
//! name in AttachScopedProbes, the scoped session's metadata persists
//! `name = Some(...)` so `list_sessions` exposes it and
//! `resolve_session("...")` finds it.

use serial_test::serial;
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
fn named_scoped_session_is_persisted_and_resolvable() {
    let _h = temp_home();
    // Defensive: clear env so the daemon-side fallback can't pollute.
    std::env::remove_var("SMELTR_SESSION_NAME");

    let ambient = Arc::new(ActiveSession::open_new().unwrap());
    let router = SessionRouter::new(ambient.clone(), None, None);

    let scoped_id = router
        .attach_scoped(
            1000,
            vec!["uv".into(), "run".into()],
            Some("TOK".into()),
            Some("my-named-run".into()),
            false,
        )
        .unwrap();
    router.detach_scoped(1000, Some(0));
    ambient.finalize(Some(0), "test").unwrap();

    // 1. Metadata on disk has the name.
    let dirs = smeltr_core::reader::list_sessions().unwrap();
    let mut found_name = false;
    for d in &dirs {
        let meta = smeltr_core::reader::read_metadata(d).unwrap();
        if meta.session_id == scoped_id {
            assert_eq!(meta.name.as_deref(), Some("my-named-run"));
            found_name = true;
        }
    }
    assert!(found_name, "scoped session metadata must carry the name");

    // 2. resolve_session_dir_by_name finds it.
    let resolved = smeltr_core::session_resolve::resolve_session_dir_by_name("my-named-run");
    assert!(
        resolved.is_some(),
        "resolve_session_dir_by_name must find the scoped session by name"
    );

    // 3. The MCP-level resolver finds it too.
    let mcp_resolved = smeltr_mcp::resolve_session("my-named-run").unwrap();
    let scoped_dir = dirs
        .iter()
        .find(|d| {
            smeltr_core::reader::read_metadata(d)
                .map(|m| m.session_id == scoped_id)
                .unwrap_or(false)
        })
        .unwrap();
    assert_eq!(&mcp_resolved, scoped_dir);
}

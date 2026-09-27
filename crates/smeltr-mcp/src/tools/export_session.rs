//! `export_session` MCP tool: write a session export to disk and return the path.

use crate::session_cache::events as read_events;
use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_analyzer::export::{to_chrome_trace, to_json_raw};
use smeltr_core::reader::read_metadata;
use smeltr_core::session::sessions_root;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    /// Session reference (short id / full UUID / name).
    pub session: String,
    /// Output format: "chrome-trace" (default) or "json".
    #[serde(default = "default_format")]
    pub format: String,
    /// Absolute output path. Its directory must already exist, and it may not
    /// lie inside the smeltr sessions store (`$SMELTR_HOME/sessions`).
    pub output_path: String,
    /// Replace `output_path` if it already exists (default false: an
    /// existing file is an error).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overwrite: Option<bool>,
}

fn default_format() -> String {
    "chrome-trace".to_string()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub path: String,
    pub bytes_written: u64,
    pub event_count: usize,
}

/// Checks `output_path` before anything is decoded (#271): exporting onto a
/// session's own `events.cbor.zst` used to succeed and leave it unreadable,
/// a relative path landed wherever the server was started, and missing
/// directories were created silently.
fn checked_output_path(raw: &str, overwrite: bool) -> Result<PathBuf, ToolError> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(ToolError::BadArgs(format!(
            "output_path must be an absolute path, got {raw:?}"
        )));
    }
    let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) else {
        return Err(ToolError::BadArgs(format!(
            "output_path must name a file, got {raw:?}"
        )));
    };
    let parent = parent.canonicalize().map_err(|_| {
        ToolError::BadArgs(format!(
            "output directory {} does not exist; create it first",
            parent.display()
        ))
    })?;
    // Resolve a symlink at the target: writing follows it.
    let target = match std::fs::symlink_metadata(path) {
        Ok(_) => path.canonicalize().map_err(|_| {
            ToolError::BadArgs(format!(
                "output_path {raw:?} is a symlink whose target cannot be resolved"
            ))
        })?,
        Err(_) => parent.join(file_name),
    };
    let store = sessions_root();
    let store = store.canonicalize().unwrap_or(store);
    if target.starts_with(&store) {
        return Err(ToolError::BadArgs(format!(
            "output_path {raw:?} is inside the smeltr sessions store {}; \
             write the export somewhere else",
            store.display()
        )));
    }
    if target.exists() && !overwrite {
        return Err(ToolError::BadArgs(format!(
            "output_path {raw:?} already exists; pass overwrite: true to replace it"
        )));
    }
    Ok(target)
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let overwrite = params.overwrite.unwrap_or(false);
    let target = checked_output_path(&params.output_path, overwrite)?;
    let dir = resolve_session(&params.session)?;
    let meta = read_metadata(&dir)?;
    let events = read_events(&dir)?;
    let event_count = events.len();

    let bytes = match params.format.as_str() {
        "chrome-trace" => to_chrome_trace(&events, &meta),
        "json" => to_json_raw(&events, &meta),
        other => {
            return Err(ToolError::BadArgs(format!(
                "unknown format {other:?}; supported: chrome-trace, json"
            )));
        }
    };

    // `create_new` closes the window between the check above and the write:
    // a file that appeared meanwhile is still not clobbered.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .create_new(!overwrite)
        .open(&target)?;
    file.write_all(bytes.as_bytes())?;

    Ok(Response {
        path: params.output_path.clone(),
        bytes_written: bytes.len() as u64,
        event_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Event, Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    fn make_minimal_session() -> SessionId {
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        for i in 0..25u64 {
            w.write_event(&Event {
                ts_mono_ns: i * 1000,
                ts_wall_ns: i * 1000,
                session_id: Uuid::nil(),
                source: Source::Mark,
                pid: None,
                seq: i,
                payload: Payload::Mark {
                    label: format!("m{i}"),
                    fields: Default::default(),
                },
            })
            .unwrap();
        }
        w.finalize(Some(0), "ok".into()).unwrap();
        id
    }

    #[test]
    #[serial_test::serial]
    fn writes_chrome_trace_and_returns_metadata() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = make_minimal_session();
        let out = home.path().join("trace.json");

        let r = run(Params {
            session: id.short(),
            format: "chrome-trace".into(),
            output_path: out.to_string_lossy().into_owned(),
            overwrite: None,
        })
        .unwrap();

        assert_eq!(r.path, out.to_string_lossy());
        assert!(r.bytes_written > 0);
        assert!(r.event_count >= 25);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert!(v["traceEvents"].is_array());
    }

    #[test]
    #[serial_test::serial]
    fn unknown_format_returns_bad_args() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = make_minimal_session();
        let out = home.path().join("trace.json");

        let err = run(Params {
            session: id.short(),
            format: "bogus".into(),
            output_path: out.to_string_lossy().into_owned(),
            overwrite: None,
        })
        .unwrap_err();
        assert!(matches!(err, ToolError::BadArgs(_)));
    }

    fn export(
        id: &SessionId,
        out: &std::path::Path,
        overwrite: Option<bool>,
    ) -> Result<Response, ToolError> {
        run(Params {
            session: id.short(),
            format: "chrome-trace".into(),
            output_path: out.to_string_lossy().into_owned(),
            overwrite,
        })
    }

    /// #271: a missing parent used to be created silently, so a typo in
    /// the path scattered directories around the disk.
    #[test]
    #[serial_test::serial]
    fn refuses_a_missing_parent_directory() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = make_minimal_session();
        let out = home.path().join("nested/deeper/trace.json");

        let err = export(&id, &out, None).unwrap_err();
        assert!(
            matches!(&err, ToolError::BadArgs(m) if m.contains("does not exist")),
            "{err:?}"
        );
        assert!(!out.parent().unwrap().exists(), "must not create dirs");
    }

    /// #271: the schema said "absolute" but a relative path was written
    /// relative to wherever the MCP server happened to be started.
    #[test]
    #[serial_test::serial]
    fn refuses_a_relative_path() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = make_minimal_session();

        let err = export(&id, std::path::Path::new("trace.json"), None).unwrap_err();
        assert!(
            matches!(&err, ToolError::BadArgs(m) if m.contains("absolute")),
            "{err:?}"
        );
        assert!(!std::path::Path::new("trace.json").exists());
    }

    /// #271: exporting onto a session's own events file succeeded and left
    /// the session unreadable. Nothing under the sessions root is writable,
    /// not even with `overwrite`.
    #[test]
    #[serial_test::serial]
    fn refuses_to_write_inside_the_sessions_root() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = make_minimal_session();
        let dir = smeltr_core::reader::find_session_dir(id).unwrap().unwrap();
        let events = dir.join("events.cbor.zst");
        let before = std::fs::read(&events).unwrap();

        for target in [
            events.clone(),
            dir.join("new.json"),
            dir.join("../x.json"),
            dir.join("../../sessions/x.json"),
        ] {
            let err = export(&id, &target, Some(true)).unwrap_err();
            assert!(
                matches!(&err, ToolError::BadArgs(m) if m.contains("sessions")),
                "{target:?}: {err:?}"
            );
        }
        assert_eq!(std::fs::read(&events).unwrap(), before);
        assert!(!dir.join("new.json").exists());
        assert!(smeltr_core::reader::read_events(&dir).unwrap().len() >= 25);
    }

    /// A symlink elsewhere that points into the store is the same file.
    #[test]
    #[serial_test::serial]
    fn refuses_a_symlink_into_the_sessions_root() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = make_minimal_session();
        let dir = smeltr_core::reader::find_session_dir(id).unwrap().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let link = elsewhere.path().join("link.json");
        std::os::unix::fs::symlink(dir.join("events.cbor.zst"), &link).unwrap();
        let before = std::fs::read(dir.join("events.cbor.zst")).unwrap();

        let err = export(&id, &link, Some(true)).unwrap_err();
        assert!(
            matches!(&err, ToolError::BadArgs(m) if m.contains("sessions")),
            "{err:?}"
        );
        assert_eq!(std::fs::read(dir.join("events.cbor.zst")).unwrap(), before);
    }

    /// An existing file is only replaced on an explicit `overwrite: true`.
    #[test]
    #[serial_test::serial]
    fn refuses_to_overwrite_unless_asked() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = make_minimal_session();
        let out = home.path().join("trace.json");
        std::fs::write(&out, "keep me").unwrap();

        for flag in [None, Some(false)] {
            let err = export(&id, &out, flag).unwrap_err();
            assert!(
                matches!(&err, ToolError::BadArgs(m) if m.contains("overwrite")),
                "{err:?}"
            );
            assert_eq!(std::fs::read_to_string(&out).unwrap(), "keep me");
        }
        let r = export(&id, &out, Some(true)).unwrap();
        assert!(r.bytes_written > 0);
        assert_ne!(std::fs::read_to_string(&out).unwrap(), "keep me");
    }
}

//! `export_session` MCP tool: write a session export to disk and return the path.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_analyzer::export::{to_chrome_trace, to_json_raw};
use smeltr_core::reader::{read_events, read_metadata};

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    /// Session reference (short id / full UUID / name).
    pub session: String,
    /// Output format: "chrome-trace" (default) or "json".
    #[serde(default = "default_format")]
    pub format: String,
    /// Absolute output path. The file is created (or overwritten).
    pub output_path: String,
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

pub fn run(params: Params) -> Result<Response, ToolError> {
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

    if let Some(parent) = std::path::Path::new(&params.output_path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::write(&params.output_path, &bytes)?;

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
        })
        .unwrap_err();
        assert!(matches!(err, ToolError::BadArgs(_)));
    }

    #[test]
    #[serial_test::serial]
    fn creates_parent_directory_when_missing() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = make_minimal_session();
        // Nested dir that does NOT exist yet.
        let out = home.path().join("nested/deeper/trace.json");
        assert!(!out.parent().unwrap().exists());

        let r = run(Params {
            session: id.short(),
            format: "chrome-trace".into(),
            output_path: out.to_string_lossy().into_owned(),
        })
        .unwrap();

        assert!(out.exists());
        assert!(r.bytes_written > 0);
    }
}

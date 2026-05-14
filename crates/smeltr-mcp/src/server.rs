//! rmcp server wiring: register tools, expose resources, run on stdio.
//!
//! The pure dispatch + resource helpers below do NOT depend on rmcp and are
//! fully testable. The actual rmcp transport in `run_stdio()` requires the
//! rmcp crate; if its API doesn't match what's coded below, `run_stdio()`
//! returns a placeholder error and the rest of the file stays valid.

use crate::tools;
use crate::types::ToolError;
use serde_json::json;

/// Dispatch a tool call by name. Used by the rmcp handler.
pub fn dispatch_call(name: &str, args: serde_json::Value) -> Result<serde_json::Value, ToolError> {
    match name {
        "list_sessions" => {
            let p: tools::list_sessions::Params = serde_json::from_value(args)?;
            let r = tools::list_sessions::run(p)?;
            Ok(serde_json::to_value(r)?)
        }
        "get_session_summary" => {
            let p: tools::session_summary::Params = serde_json::from_value(args)?;
            let r = tools::session_summary::run(p)?;
            Ok(serde_json::to_value(r)?)
        }
        "query_events" => {
            let p: tools::query_events::Params = serde_json::from_value(args)?;
            let r = tools::query_events::run(p)?;
            Ok(serde_json::to_value(r)?)
        }
        "find_correlations" => {
            let p: tools::correlations::Params = serde_json::from_value(args)?;
            let r = tools::correlations::run(p)?;
            Ok(serde_json::to_value(r)?)
        }
        "get_crash_report" => {
            let p: tools::crash_report::Params = serde_json::from_value(args)?;
            let r = tools::crash_report::run(p)?;
            Ok(serde_json::to_value(r)?)
        }
        "get_metal_cb_history" => {
            let p: tools::metal_cb_history::Params = serde_json::from_value(args)?;
            let r = tools::metal_cb_history::run(p)?;
            Ok(serde_json::to_value(r)?)
        }
        "compare_sessions" => {
            let p: tools::compare_sessions::Params = serde_json::from_value(args)?;
            let r = tools::compare_sessions::run(p)?;
            Ok(serde_json::to_value(r)?)
        }
        other => Err(ToolError::BadArgs(format!("unknown tool {other:?}"))),
    }
}

/// Returns the list of `smeltr://session/<dir_name>` resources.
pub fn list_session_resource_uris() -> Result<Vec<String>, ToolError> {
    let dirs = smeltr_core::reader::list_sessions()?;
    Ok(dirs
        .into_iter()
        .filter_map(|d| {
            d.file_name()
                .and_then(|n| n.to_str())
                .map(|n| format!("smeltr://session/{n}"))
        })
        .collect())
}

/// Reads a session resource URI `smeltr://session/<dir_name>` and returns
/// `{ metadata, events }` as JSON.
pub fn read_session_resource(uri: &str) -> Result<serde_json::Value, ToolError> {
    let dir_name = uri
        .strip_prefix("smeltr://session/")
        .ok_or_else(|| ToolError::BadArgs(format!("not a smeltr URI: {uri:?}")))?;
    let dirs = smeltr_core::reader::list_sessions()?;
    let dir = dirs
        .into_iter()
        .find(|d| d.file_name().and_then(|n| n.to_str()) == Some(dir_name))
        .ok_or_else(|| ToolError::NotFound(uri.to_string()))?;
    let metadata = smeltr_core::reader::read_metadata(&dir).ok();
    let events = smeltr_core::reader::read_events(&dir)?;
    Ok(json!({
        "metadata": metadata,
        "events": events,
    }))
}

/// Runs the MCP server on stdio.
pub async fn run_stdio() -> std::io::Result<()> {
    // TASK 5 SECOND HALF: rmcp API adaptation deferred. The pure-Rust
    // dispatch + resource helpers above are wired and tested. Once the rmcp
    // ServerHandler API surface is mapped, this function should construct a
    // handler that delegates list_tools / call_tool to `dispatch_call` and
    // list_resources / read_resource to the helpers above, then serve on
    // stdio (rmcp::transport::stdio or rmcp::serve_server).
    Err(std::io::Error::other(
        "smeltr-mcp::server::run_stdio is not wired to rmcp yet",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;

    #[test]
    #[serial_test::serial]
    fn dispatch_list_sessions_returns_json() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let v = dispatch_call("list_sessions", json!({})).unwrap();
        assert!(v.get("sessions").is_some());
    }

    #[test]
    #[serial_test::serial]
    fn unknown_tool_is_bad_args() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let r = dispatch_call("nope", json!({}));
        assert!(matches!(r, Err(ToolError::BadArgs(_))));
    }

    #[test]
    #[serial_test::serial]
    fn list_session_resource_uris_uses_smeltr_scheme() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let w = SessionWriter::create(meta).unwrap();
        drop(w);
        let uris = list_session_resource_uris().unwrap();
        assert_eq!(uris.len(), 1);
        assert!(uris[0].starts_with("smeltr://session/"));
    }

    #[test]
    #[serial_test::serial]
    fn read_session_resource_returns_metadata_and_events() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let w = SessionWriter::create(meta).unwrap();
        let dir_name = w.dir().file_name().unwrap().to_string_lossy().to_string();
        drop(w);
        let v = read_session_resource(&format!("smeltr://session/{dir_name}")).unwrap();
        assert!(v.get("metadata").is_some());
        assert!(v.get("events").is_some());
    }
}

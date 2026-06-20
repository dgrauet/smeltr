//! rmcp server wiring: register tools, expose resources, run on stdio.
//!
//! The pure-Rust `dispatch_call` / resource helpers do NOT depend on rmcp and
//! are fully unit-testable. The `ServerHandler` impl below adapts them to the
//! rmcp model types, and `run_stdio()` serves them over stdio.

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
        "get_inference_breakdown" => {
            let p: tools::inference_breakdown::Params = serde_json::from_value(args)?;
            let r = tools::inference_breakdown::run(p)?;
            Ok(serde_json::to_value(r)?)
        }
        "export_session" => {
            let p: tools::export_session::Params = serde_json::from_value(args)?;
            let r = tools::export_session::run(p)?;
            Ok(serde_json::to_value(r)?)
        }
        "get_op_summary" => {
            let p: tools::op_summary::Params = serde_json::from_value(args)?;
            let r = tools::op_summary::run(p)?;
            Ok(serde_json::to_value(r)?)
        }
        "get_dispatch_origins" => {
            let p: tools::dispatch_origins::Params = serde_json::from_value(args)?;
            let r = tools::dispatch_origins::run(p)?;
            Ok(serde_json::to_value(r)?)
        }
        "get_memory_breakdown" => {
            let p: tools::memory_breakdown::Params = serde_json::from_value(args)?;
            let r = tools::memory_breakdown::run(p)?;
            Ok(serde_json::to_value(r)?)
        }
        "get_model_loads" => {
            let p: tools::model_loads::Params = serde_json::from_value(args)?;
            let r = tools::model_loads::run(p)?;
            Ok(serde_json::to_value(r)?)
        }
        "subscribe_live" => {
            let p: tools::subscribe_live::Params = serde_json::from_value(args)?;
            let r = tools::subscribe_live::run(p)?;
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

// -- rmcp wiring ------------------------------------------------------------

use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, ErrorData as McpError, Implementation,
    JsonObject, ListResourcesResult, ListToolsResult, PaginatedRequestParams, RawResource,
    ReadResourceRequestParams, ReadResourceResult, Resource, ResourceContents, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};

/// Server handler implementing the 7 smeltr tools and `smeltr://session/*`
/// resources. Stateless — pure-Rust dispatch delegates to module helpers.
#[derive(Clone, Default)]
pub struct SmeltrMcpServer;

fn schema_for<T: schemars::JsonSchema>() -> Arc<JsonObject> {
    let v = serde_json::to_value(schemars::schema_for!(T)).unwrap_or_default();
    let map = match v {
        serde_json::Value::Object(m) => m,
        _ => JsonObject::new(),
    };
    Arc::new(map)
}

fn tool<T: schemars::JsonSchema>(name: &'static str, description: &'static str) -> Tool {
    Tool::new(name, description, schema_for::<T>())
}

fn tool_error_to_mcp(e: ToolError) -> McpError {
    match e {
        ToolError::NotFound(s) => McpError::resource_not_found(format!("not found: {s}"), None),
        ToolError::BadArgs(s) => McpError::invalid_params(s, None),
        ToolError::Serde(s) => McpError::invalid_params(s.to_string(), None),
        ToolError::Io(s) => McpError::internal_error(s.to_string(), None),
    }
}

fn tool_list() -> Vec<Tool> {
    vec![
        tool::<crate::tools::list_sessions::Params>(
            "list_sessions",
            "List recorded smeltr sessions.",
        ),
        tool::<crate::tools::session_summary::Params>(
            "get_session_summary",
            "Summarize a session: counts, time range, root cause.",
        ),
        tool::<crate::tools::query_events::Params>(
            "query_events",
            "Query events from a session with filters (source, kind, limit).",
        ),
        tool::<crate::tools::correlations::Params>(
            "find_correlations",
            "Find correlated events in a session via the analyzer.",
        ),
        tool::<crate::tools::crash_report::Params>(
            "get_crash_report",
            "Retrieve crash reports captured during a session.",
        ),
        tool::<crate::tools::metal_cb_history::Params>(
            "get_metal_cb_history",
            "Retrieve Metal command-buffer history events for a session.",
        ),
        tool::<crate::tools::compare_sessions::Params>(
            "compare_sessions",
            "Compare two sessions side by side.",
        ),
        tool::<crate::tools::inference_breakdown::Params>(
            "get_inference_breakdown",
            "Per-module GPU time breakdown for an MLX inference session.",
        ),
        tool::<crate::tools::op_summary::Params>(
            "get_op_summary",
            "Flat cross-module aggregation of GPU time per op kind (Matmul, Softmax, ...).",
        ),
        tool::<crate::tools::dispatch_origins::Params>(
            "get_dispatch_origins",
            "Per-(kind, file:line) GPU time attribution. Requires sessions recorded with SMELTR_STACK_CAPTURE=1.",
        ),
        tool::<crate::tools::memory_breakdown::Params>(
            "get_memory_breakdown",
            "Per-scope MTLDevice memory peak/avg/end and per-scope live-heap peak (count + bytes).",
        ),
        tool::<crate::tools::export_session::Params>(
            "export_session",
            "Export a recorded session to chrome-trace JSON (openable in chrome://tracing / Perfetto / Speedscope) or raw JSON. Writes to disk and returns the file path.",
        ),
        tool::<crate::tools::model_loads::Params>(
            "get_model_loads",
            "List all model loads in a session with duplicate detection. Returns each load with duration_ns and a duplicate_of index when the same path was loaded more than once.",
        ),
        tool::<crate::tools::subscribe_live::Params>(
            "subscribe_live",
            "Poll a running session for a delta summary of activity since a cursor (live tail). Returns counts by payload, GPU time + top op kinds, current/peak memory, and model loads for events after `cursor`; pass the returned `cursor` back next poll. Omit `session` to target the most-recent live session, then pass the returned `session_id` as `session` on every later poll to stay bound to it. This is a turn-based poll, not a push stream.",
        ),
    ]
}

impl ServerHandler for SmeltrMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new("smeltr-mcp", env!("CARGO_PKG_VERSION")))
        .with_instructions(
            "smeltr MCP server: query Metal/MLX observability sessions on macOS Apple Silicon.\n\
             \n\
             Typical workflow: (1) `list_sessions` to find sessions; (2) `get_session_summary` \
             for a quick overview; (3) drill in with `get_inference_breakdown` (per-scope GPU \
             time tree), `get_op_summary` (flat ops by kind), `get_memory_breakdown` (per-scope \
             peak/avg/end + heap), or `get_dispatch_origins` (per file:line attribution, requires \
             SMELTR_STACK_CAPTURE=1 at record time); (4) `compare_sessions` for A/B regression \
             analysis across scopes/ops/memory/origins; (5) `export_session` to dump chrome-trace \
             JSON openable in chrome://tracing, Perfetto, or Speedscope.\n\
             \n\
             For raw access: `query_events` (filtered event stream), `get_metal_cb_history` \
             (Metal command-buffer events), `get_crash_report` (crash dumps), `find_correlations` \
             (deterministic analyzer findings).\n\
             \n\
             Session refs accept short id (8 hex), full UUID, or SessionMetadata.name. Sessions \
             are recorded via `smeltr record -- <cmd>`; the optional Python sidecar adds \
             `smeltr.scope(\"name\")` for semantic GPU-time attribution.",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tool_list()))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let args = request
            .arguments
            .map(serde_json::Value::Object)
            .unwrap_or(serde_json::Value::Object(JsonObject::new()));
        match dispatch_call(&request.name, args) {
            Ok(value) => {
                let text = serde_json::to_string(&value)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                let mut result = CallToolResult::success(vec![Content::text(text)]);
                result.structured_content = Some(value);
                Ok(result)
            }
            Err(e) => Err(tool_error_to_mcp(e)),
        }
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let uris = list_session_resource_uris().map_err(tool_error_to_mcp)?;
        use rmcp::model::AnnotateAble;
        let resources: Vec<Resource> = uris
            .into_iter()
            .map(|uri| {
                let name = uri
                    .strip_prefix("smeltr://session/")
                    .unwrap_or(&uri)
                    .to_string();
                RawResource::new(uri, name)
                    .with_mime_type("application/json")
                    .no_annotation()
            })
            .collect();
        Ok(ListResourcesResult::with_all_items(resources))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        let value = read_session_resource(&request.uri).map_err(tool_error_to_mcp)?;
        let text = serde_json::to_string(&value)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            text,
            request.uri,
        )
        .with_mime_type("application/json")]))
    }

    async fn on_initialized(&self, _context: NotificationContext<RoleServer>) {}
}

/// Runs the MCP server on stdio.
pub async fn run_stdio() -> std::io::Result<()> {
    use rmcp::ServiceExt;
    let transport = rmcp::transport::stdio();
    let service = SmeltrMcpServer
        .serve(transport)
        .await
        .map_err(std::io::Error::other)?;
    service.waiting().await.map_err(std::io::Error::other)?;
    Ok(())
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

    #[test]
    #[serial_test::serial]
    fn dispatch_subscribe_live_returns_summary() {
        use smeltr_core::session::{SessionId, SessionMetadata};
        use smeltr_core::writer::SessionWriter;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let mut w = SessionWriter::create(SessionMetadata::now_starting(id)).unwrap();
        w.write_event(&smeltr_core::event::Event {
            ts_mono_ns: 0,
            ts_wall_ns: 0,
            session_id: uuid::Uuid::nil(),
            source: smeltr_core::event::Source::Mark,
            pid: None,
            seq: 0,
            payload: smeltr_core::event::Payload::Mark {
                label: "x".into(),
                fields: Default::default(),
            },
        })
        .unwrap();
        w.flush().unwrap();

        let args = serde_json::json!({ "session": id.short() });
        let v = dispatch_call("subscribe_live", args).unwrap();
        assert_eq!(v.get("new_events").and_then(|x| x.as_u64()), Some(1));
        assert_eq!(v.get("live").and_then(|x| x.as_bool()), Some(true));
    }

    #[test]
    fn unknown_tool_rejected() {
        // Unknown-tool guard still works and the new arm compiles into dispatch.
        let err = dispatch_call("definitely_not_a_tool", serde_json::json!({}));
        assert!(err.is_err());
    }

    #[test]
    fn subscribe_live_in_tool_list() {
        assert!(tool_list().iter().any(|t| t.name == "subscribe_live"));
    }
}

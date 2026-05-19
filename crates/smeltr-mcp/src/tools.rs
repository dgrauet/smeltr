//! MCP tools. Pure logic; no rmcp/serde-rpc plumbing.

pub mod compare_sessions;
pub mod correlations;
pub mod crash_report;
pub mod dispatch_origins;
pub mod export_session;
pub mod inference_breakdown;
pub mod list_sessions;
pub mod memory_breakdown;
pub mod metal_cb_history;
pub mod op_summary;
pub mod query_events;
pub mod session_summary;

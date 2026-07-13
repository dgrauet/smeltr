//! MCP server for smeltr sessions.

pub mod server;
pub mod tools;
pub mod types;

#[cfg(feature = "http")]
pub mod http;

pub use server::run_stdio;
pub use types::{resolve_session, SessionRef, ToolError};

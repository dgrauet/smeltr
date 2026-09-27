//! MCP server for smeltr sessions.

pub mod budget;
pub mod server;
pub mod session_cache;
pub mod tools;
pub mod types;

#[cfg(test)]
mod test_util;

#[cfg(feature = "http")]
pub mod http;

pub use server::run_stdio;
pub use types::{resolve_session, SessionRef, ToolError};

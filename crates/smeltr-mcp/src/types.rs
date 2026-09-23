//! Shared types for MCP tools.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Identifies a session on disk: short id, full UUID, directory name or
/// `SessionMetadata.name` (see [`resolve_session`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRef {
    pub id: String,
}

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("session {0:?} not found")]
    NotFound(String),
    #[error("invalid arguments: {0}")]
    BadArgs(String),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Resolve a session ref to a directory path with the rules every surface
/// shares ([`smeltr_core::session_resolve::resolve_session`]: short id, full
/// UUID, directory name or `SessionMetadata.name`), mapped onto `ToolError`.
pub fn resolve_session(arg: &str) -> Result<std::path::PathBuf, ToolError> {
    use smeltr_core::session_resolve::ResolveError;
    smeltr_core::session_resolve::resolve_session(arg).map_err(|e| match e {
        ResolveError::Io(e) => ToolError::Io(e),
        ResolveError::NotFound(s) => ToolError::NotFound(s),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapper's only job is mapping `ResolveError` onto `ToolError`;
    /// the lookup rules themselves are tested in `smeltr_core::session_resolve`.
    #[test]
    #[serial_test::serial]
    fn resolve_maps_not_found_onto_tool_error() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        assert!(matches!(
            resolve_session("abc"),
            Err(ToolError::NotFound(_))
        ));
    }
}

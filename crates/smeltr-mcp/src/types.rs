//! Shared types for MCP tools.

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Identifies a session on disk. Accepts a directory-name suffix match
/// (e.g. the 8-char short id) or the full directory name.
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

/// Resolve a session ref (suffix match) to a directory path. Returns the most
/// recent matching session if multiple match.
pub fn resolve_session(arg: &str) -> Result<std::path::PathBuf, ToolError> {
    let sessions = smeltr_core::reader::list_sessions()?;
    if sessions.is_empty() {
        return Err(ToolError::NotFound(arg.to_string()));
    }
    for dir in sessions.iter().rev() {
        if dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.contains(arg))
            .unwrap_or(false)
        {
            return Ok(dir.clone());
        }
    }
    Err(ToolError::NotFound(arg.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;

    #[test]
    #[serial_test::serial]
    fn resolve_returns_not_found_when_empty() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        assert!(matches!(
            resolve_session("abc"),
            Err(ToolError::NotFound(_))
        ));
    }

    #[test]
    #[serial_test::serial]
    fn resolve_finds_by_short_id_suffix() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        drop(w);

        let resolved = resolve_session(&id.short()).unwrap();
        assert_eq!(resolved, dir);
    }
}

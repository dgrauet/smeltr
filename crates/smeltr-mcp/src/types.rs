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

/// Resolve a session ref to a directory path. Tries (in order):
///   1. Directory-name suffix match (short id / partial). Returns the
///      most recent matching session.
///   2. Full-UUID match against `metadata.session_id` (for callers that
///      pass back the full UUID returned by a previous call).
///   3. Exact `SessionMetadata.name` match across all sessions
///      (`smeltr_core::session_resolve::resolve_session_dir_by_name`),
///      most-recent wins.
///
/// Returns `NotFound` if no path matches.
pub fn resolve_session(arg: &str) -> Result<std::path::PathBuf, ToolError> {
    let sessions = smeltr_core::reader::list_sessions()?;
    if !sessions.is_empty() {
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
    }
    // Full-UUID match: a 32-hex (or dashed) UUID does not appear in the
    // short-id-based directory name, so match it against metadata.session_id.
    if let Ok(want) = arg.parse::<smeltr_core::session::SessionId>() {
        for dir in sessions.iter().rev() {
            if smeltr_core::reader::read_metadata(dir)
                .map(|m| m.session_id == want)
                .unwrap_or(false)
            {
                return Ok(dir.clone());
            }
        }
    }
    if let Some(dir) = smeltr_core::session_resolve::resolve_session_dir_by_name(arg) {
        return Ok(dir);
    }
    Err(ToolError::NotFound(arg.to_string()))
}

/// Most recently started recording (directory names sort chronologically:
/// `YYYY-MM-DD-HHMMSS-<short>`). Ambient sessions are skipped — the daemon
/// reopens one at every boot, so right after a daemon restart the newest
/// directory is an (empty) ambient session, not the run the user means by
/// "last". Falls back to the newest session of any kind when no non-ambient
/// session exists. `NotFound("<latest>")` when there is none at all. Used
/// by CLI `--last` flags to skip the list-then-copy-paste dance.
pub fn latest_session() -> Result<std::path::PathBuf, ToolError> {
    let sessions = smeltr_core::reader::list_sessions()?;
    for dir in sessions.iter().rev() {
        let is_ambient = smeltr_core::reader::read_metadata(dir)
            .map(|m| matches!(m.kind, smeltr_core::session::SessionKind::Ambient))
            .unwrap_or(false);
        if !is_ambient {
            return Ok(dir.clone());
        }
    }
    sessions
        .into_iter()
        .next_back()
        .ok_or_else(|| ToolError::NotFound("<latest>".to_string()))
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

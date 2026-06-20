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
///   2. Exact `SessionMetadata.name` match across all sessions
///      (`smeltr_core::session_resolve::resolve_session_dir_by_name`),
///      most-recent wins.
///
/// Returns `NotFound` if neither path matches.
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

    #[test]
    #[serial_test::serial]
    fn resolve_finds_by_name() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.name = Some("ltx2-experiment".into());
        let w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        drop(w);

        let resolved = resolve_session("ltx2-experiment").unwrap();
        assert_eq!(resolved, dir);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_short_id_wins_over_name() {
        // Hard collision: a session whose name == another session's short id.
        // The short-id (suffix) match must fire first.
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());

        let id_real = SessionId::new();
        let short = id_real.short();
        let meta_real = SessionMetadata::now_starting(id_real);
        let w_real = SessionWriter::create(meta_real).unwrap();
        let dir_real = w_real.dir().to_path_buf();
        drop(w_real);

        let id_decoy = SessionId::new();
        let mut meta_decoy = SessionMetadata::now_starting(id_decoy);
        meta_decoy.name = Some(short.clone());
        let w_decoy = SessionWriter::create(meta_decoy).unwrap();
        drop(w_decoy);

        // Resolution with `short` should hit the real session via suffix match,
        // not the decoy session via name.
        let resolved = resolve_session(&short).unwrap();
        assert_eq!(resolved, dir_real);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_finds_by_full_uuid() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let dir = SessionWriter::create(meta).unwrap().dir().to_path_buf();
        let found = resolve_session(&id.to_string()).unwrap();
        assert_eq!(found, dir);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_unknown_name_returns_not_found() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let _w = SessionWriter::create(meta).unwrap();
        assert!(matches!(
            resolve_session("nonexistent-name"),
            Err(ToolError::NotFound(_))
        ));
    }
}

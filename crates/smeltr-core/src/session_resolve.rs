//! Resolve a session by its user-given `name` (most-recent wins).
//!
//! Used by `smeltr-mcp::resolve_session` and the CLI session resolver as
//! the third lookup form alongside short-id suffix match and full UUID.

use crate::reader::{list_sessions, read_metadata};
use std::path::PathBuf;

/// Find the most recent session directory whose `meta.toml` has
/// `name == Some(name)`. Returns `None` if no session matches.
///
/// "Most recent" is determined by `started_rfc3339`, descending. Ties
/// are broken by directory name (descending) for determinism.
pub fn resolve_session_dir_by_name(name: &str) -> Option<PathBuf> {
    let dirs = list_sessions().ok()?;
    let mut matches: Vec<(String, PathBuf)> = dirs
        .into_iter()
        .filter_map(|dir| {
            let meta = read_metadata(&dir).ok()?;
            if meta.name.as_deref() == Some(name) {
                Some((meta.started_rfc3339, dir))
            } else {
                None
            }
        })
        .collect();
    matches.sort_by(|(ts_a, dir_a), (ts_b, dir_b)| {
        ts_b.cmp(ts_a)
            .then_with(|| dir_b.file_name().cmp(&dir_a.file_name()))
    });
    matches.into_iter().next().map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionId, SessionMetadata};
    use crate::writer::SessionWriter;

    fn session_with_name(name: &str) -> PathBuf {
        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.name = Some(name.into());
        let w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        drop(w);
        dir
    }

    fn session_no_name() -> PathBuf {
        let id = SessionId::new();
        // Defensive: clear env so now_starting doesn't pick up a leftover.
        std::env::remove_var("SMELTR_SESSION_NAME");
        let meta = SessionMetadata::now_starting(id);
        let w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        drop(w);
        dir
    }

    #[test]
    #[serial_test::serial]
    fn returns_none_when_no_match() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let _ = session_with_name("alpha");
        assert!(resolve_session_dir_by_name("beta").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn returns_none_when_no_sessions_at_all() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        assert!(resolve_session_dir_by_name("anything").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn matches_exact_name() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let dir = session_with_name("alpha");
        assert_eq!(resolve_session_dir_by_name("alpha"), Some(dir));
    }

    #[test]
    #[serial_test::serial]
    fn ignores_sessions_without_name() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let _ = session_no_name();
        let dir = session_with_name("alpha");
        assert_eq!(resolve_session_dir_by_name("alpha"), Some(dir));
    }

    #[test]
    #[serial_test::serial]
    fn most_recent_wins_on_collision() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        // Older session via the normal path.
        let _older = session_with_name("dup");
        // Newer session: construct metadata manually with a forced-later
        // timestamp to keep the test deterministic and instantaneous.
        let mut meta = SessionMetadata::now_starting(SessionId::new());
        meta.name = Some("dup".into());
        meta.started_rfc3339 = "2099-01-01T00:00:00Z".into();
        let w = SessionWriter::create(meta).unwrap();
        let newer = w.dir().to_path_buf();
        drop(w);
        assert_eq!(resolve_session_dir_by_name("dup"), Some(newer));
    }
}

//! Turn a user-typed session reference into a directory on disk.
//!
//! Every surface accepts the same three forms — short id, full UUID, or
//! `SessionMetadata.name` — so the rule that maps them to a directory lives
//! here, next to the on-disk format it reads, rather than in whichever crate
//! happened to need it first. `smeltr-mcp` and `smeltr-cli` each wrap
//! [`resolve_session`] in their own error type.

use crate::reader::{list_sessions, read_metadata};
use crate::session::{SessionId, SessionKind};
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("session {0:?} not found")]
    NotFound(String),
}

/// Resolve a session ref to a directory path. Tries (in order):
///   1. Directory-name suffix match (short id / partial). Returns the
///      most recent matching session.
///   2. Full-UUID match against `metadata.session_id` (for callers that
///      pass back the full UUID returned by a previous call).
///   3. Exact `SessionMetadata.name` match, most-recent wins
///      ([`resolve_session_dir_by_name`]).
pub fn resolve_session(arg: &str) -> Result<PathBuf, ResolveError> {
    let sessions = list_sessions()?;
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
    // Full-UUID match: a 32-hex (or dashed) UUID does not appear in the
    // short-id-based directory name, so match it against metadata.session_id.
    if let Ok(want) = arg.parse::<SessionId>() {
        for dir in sessions.iter().rev() {
            if read_metadata(dir)
                .map(|m| m.session_id == want)
                .unwrap_or(false)
            {
                return Ok(dir.clone());
            }
        }
    }
    resolve_session_dir_by_name(arg).ok_or_else(|| ResolveError::NotFound(arg.to_string()))
}

/// Most recently started recording (directory names sort chronologically:
/// `YYYY-MM-DD-HHMMSS-<short>`). Ambient sessions are skipped — the daemon
/// reopens one at every boot, so right after a daemon restart the newest
/// directory is an (empty) ambient session, not the run the user means by
/// "last". Falls back to the newest session of any kind when no non-ambient
/// session exists. `NotFound("<latest>")` when there is none at all. Used
/// by CLI `--last` flags to skip the list-then-copy-paste dance.
pub fn latest_session() -> Result<PathBuf, ResolveError> {
    let sessions = list_sessions()?;
    for dir in sessions.iter().rev() {
        let is_ambient = read_metadata(dir)
            .map(|m| matches!(m.kind, SessionKind::Ambient))
            .unwrap_or(false);
        if !is_ambient {
            return Ok(dir.clone());
        }
    }
    sessions
        .into_iter()
        .next_back()
        .ok_or_else(|| ResolveError::NotFound("<latest>".to_string()))
}

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

    #[test]
    #[serial_test::serial]
    fn resolve_returns_not_found_when_empty() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        assert!(matches!(
            resolve_session("abc"),
            Err(ResolveError::NotFound(_))
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
            Err(ResolveError::NotFound(_))
        ));
    }

    #[test]
    #[serial_test::serial]
    fn latest_session_returns_most_recent() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());

        let mut meta_old = SessionMetadata::now_starting(SessionId::new());
        meta_old.started_rfc3339 = "2026-07-14T10:00:00Z".into();
        drop(SessionWriter::create(meta_old).unwrap());

        let mut meta_new = SessionMetadata::now_starting(SessionId::new());
        meta_new.started_rfc3339 = "2026-07-15T09:30:00Z".into();
        let w = SessionWriter::create(meta_new).unwrap();
        let dir_new = w.dir().to_path_buf();
        drop(w);

        assert_eq!(latest_session().unwrap(), dir_new);
    }

    /// The daemon reopens an ambient session at every boot: right after a
    /// restart the newest directory is that (empty) ambient session, not
    /// the recording the user means by "last" — it must be skipped.
    #[test]
    #[serial_test::serial]
    fn latest_session_skips_newer_ambient() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());

        let mut meta_run = SessionMetadata::now_starting(SessionId::new());
        meta_run.started_rfc3339 = "2026-07-15T09:30:00Z".into();
        meta_run.kind = crate::session::SessionKind::Scoped {
            pid: 1234,
            argv: vec!["ltx".into()],
        };
        let w = SessionWriter::create(meta_run).unwrap();
        let dir_run = w.dir().to_path_buf();
        drop(w);

        let mut meta_ambient = SessionMetadata::now_starting(SessionId::new());
        meta_ambient.started_rfc3339 = "2026-07-15T09:58:00Z".into();
        meta_ambient.kind = crate::session::SessionKind::Ambient;
        drop(SessionWriter::create(meta_ambient).unwrap());

        assert_eq!(latest_session().unwrap(), dir_run);
    }

    /// With only ambient sessions on disk, fall back to the newest one
    /// rather than erroring.
    #[test]
    #[serial_test::serial]
    fn latest_session_falls_back_to_ambient_when_alone() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());

        let mut meta = SessionMetadata::now_starting(SessionId::new());
        meta.started_rfc3339 = "2026-07-15T09:58:00Z".into();
        meta.kind = crate::session::SessionKind::Ambient;
        let w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        drop(w);

        assert_eq!(latest_session().unwrap(), dir);
    }

    #[test]
    #[serial_test::serial]
    fn latest_session_not_found_when_empty() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        assert!(matches!(latest_session(), Err(ResolveError::NotFound(_))));
    }
}

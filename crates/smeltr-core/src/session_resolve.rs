//! Turn a user-typed session reference into a directory on disk.
//!
//! Every surface accepts the same forms — short id, full UUID, directory
//! name, or `SessionMetadata.name` — so the rule that maps them to a directory lives
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

/// Every session directory, newest first by `started_rfc3339`.
///
/// Directory names do not sort by age: every `post-mortem-<label>-…`
/// directory sorts after every dated `YYYY-MM-DD-…` one, and post-mortems
/// among themselves sort by label first (#241). Sessions whose metadata is
/// unreadable come last; ties fall back to the directory name, descending.
pub fn sessions_newest_first() -> std::io::Result<Vec<PathBuf>> {
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;

    let mut keyed: Vec<(Option<OffsetDateTime>, PathBuf)> = list_sessions()?
        .into_iter()
        .map(|dir| {
            let started = read_metadata(&dir)
                .ok()
                .and_then(|m| OffsetDateTime::parse(&m.started_rfc3339, &Rfc3339).ok());
            (started, dir)
        })
        .collect();
    // `None < Some(_)`, so a descending sort puts unreadable sessions last.
    keyed.sort_by(|(ta, da), (tb, db)| tb.cmp(ta).then_with(|| db.cmp(da)));
    Ok(keyed.into_iter().map(|(_, dir)| dir).collect())
}

/// Whether `arg` names `dir`: its full directory name, or its 8-hex short
/// id (the directory's last `-`-separated component). Deliberately not a
/// substring match — directory names are mostly digits, so a substring
/// match let any numeric session name resolve to an unrelated session.
fn names_directory(dir: &std::path::Path, arg: &str) -> bool {
    let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    name == arg
        || (arg.len() == 8
            && arg.bytes().all(|b| b.is_ascii_hexdigit())
            && name.rsplit('-').next() == Some(arg))
}

/// Resolve a session ref to a directory path. Tries (in order):
///   1. Full directory name or 8-hex short id; newest session wins.
///   2. Full-UUID match against `metadata.session_id` (for callers that
///      pass back the full UUID returned by a previous call).
///   3. Exact `SessionMetadata.name` match, most-recent wins
///      ([`resolve_session_dir_by_name`]).
pub fn resolve_session(arg: &str) -> Result<PathBuf, ResolveError> {
    let sessions = sessions_newest_first()?;
    if let Some(dir) = sessions.iter().find(|d| names_directory(d, arg)) {
        return Ok(dir.clone());
    }
    // Full-UUID match: a 32-hex (or dashed) UUID does not appear in the
    // short-id-based directory name, so match it against metadata.session_id.
    if let Ok(want) = arg.parse::<SessionId>() {
        for dir in &sessions {
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

/// Most recently started recording. Ambient sessions (including
/// post-mortems, whose metadata carries no kind) are skipped — the daemon
/// reopens one at every boot, so right after a daemon restart the newest
/// session is an (empty) ambient one, not the run the user means by
/// "last". Falls back to the newest session of any kind when no
/// non-ambient session exists. `NotFound("<latest>")` when there is none
/// at all. Backs every CLI `--last` flag.
pub fn latest_session() -> Result<PathBuf, ResolveError> {
    let sessions = sessions_newest_first()?;
    let recording = sessions.iter().find(|dir| {
        read_metadata(dir)
            .map(|m| !matches!(m.kind, SessionKind::Ambient))
            .unwrap_or(false)
    });
    recording
        .or_else(|| sessions.first())
        .cloned()
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

    /// #241: a name must not be shadowed by another session whose
    /// directory name merely contains it (timestamps are full of digits).
    #[test]
    #[serial_test::serial]
    fn resolve_name_is_not_shadowed_by_a_directory_substring() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let mut named = SessionMetadata::now_starting(SessionId::new());
        named.started_rfc3339 = "2026-07-01T08:00:00Z".into();
        named.name = Some("15".into());
        let named_dir = SessionWriter::create(named).unwrap().dir().to_path_buf();
        let mut other = SessionMetadata::now_starting(SessionId::new());
        other.started_rfc3339 = "2026-07-01T10:15:30Z".into(); // dir …-101530-…
        drop(SessionWriter::create(other).unwrap());

        assert_eq!(resolve_session("15").unwrap(), named_dir);
    }

    #[test]
    #[serial_test::serial]
    fn resolve_rejects_an_empty_ref() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        drop(SessionWriter::create(SessionMetadata::now_starting(SessionId::new())).unwrap());
        assert!(matches!(
            resolve_session(""),
            Err(ResolveError::NotFound(_))
        ));
    }

    #[test]
    #[serial_test::serial]
    fn resolve_accepts_a_full_directory_name() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let dir = SessionWriter::create(SessionMetadata::now_starting(SessionId::new()))
            .unwrap()
            .dir()
            .to_path_buf();
        let name = dir.file_name().unwrap().to_str().unwrap().to_string();
        assert_eq!(resolve_session(&name).unwrap(), dir);
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

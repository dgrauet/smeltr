//! Resolve a session directory from CLI arguments, preferring Scoped sessions
//! over the daemon's Ambient session by default.

use anyhow::{anyhow, Context, Result};
use smeltr_core::reader::{list_sessions, read_metadata};
use smeltr_core::session::SessionKind;
use std::path::PathBuf;

/// Pick the session directory the user most likely means.
///
/// Resolution order:
/// 1. If `id` is given, resolve it like every other session arg
///    (`smeltr_mcp::types::resolve_session`: short id, full UUID, or
///    `SessionMetadata.name` — #116).
/// 2. Else if `prefer_post_mortem` is true, look for a `post-mortem-` dir first.
/// 3. Else if `include_ambient` is true, return the newest session of any kind.
/// 4. Else return the newest Scoped session, falling back to newest overall.
pub fn resolve(
    id: Option<String>,
    prefer_post_mortem: bool,
    include_ambient: bool,
) -> Result<PathBuf> {
    if let Some(id) = id {
        return smeltr_mcp::types::resolve_session(&id)
            .map_err(|e| anyhow!("could not resolve session {id:?}: {e}"));
    }
    let sessions = list_sessions().context("listing sessions")?;
    if sessions.is_empty() {
        return Err(anyhow!("no sessions found under SMELTR_HOME"));
    }
    if prefer_post_mortem {
        if let Some(pm) = sessions.iter().rev().find(|d| {
            d.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("post-mortem-"))
                .unwrap_or(false)
        }) {
            return Ok(pm.clone());
        }
    }
    if include_ambient {
        return sessions
            .last()
            .cloned()
            .ok_or_else(|| anyhow!("no sessions found"));
    }
    // Prefer newest Scoped.
    for dir in sessions.iter().rev() {
        if let Ok(meta) = read_metadata(dir) {
            if matches!(meta.kind, SessionKind::Scoped { .. }) {
                return Ok(dir.clone());
            }
        }
    }
    // Fallback: newest of any kind.
    sessions
        .last()
        .cloned()
        .ok_or_else(|| anyhow!("no sessions found"))
}

/// Common `<SESSION> | --last` resolution for subcommands where the two are
/// mutually exclusive (enforced by clap at the arg level).
pub fn resolve_arg(session: Option<&str>, last: bool) -> Result<PathBuf> {
    if last {
        resolve(None, false, false)
    } else {
        resolve(Some(session.unwrap_or_default().to_string()), false, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;

    fn make_session(home_path: &std::path::Path, kind: SessionKind) -> PathBuf {
        let _ = home_path;
        let mut meta = SessionMetadata::now_starting(SessionId::new());
        meta.kind = kind;
        let w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "x".into()).unwrap();
        dir
    }

    #[test]
    #[serial]
    fn prefers_scoped_over_ambient() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let amb_dir = make_session(home.path(), SessionKind::Ambient);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let sc_dir = make_session(
            home.path(),
            SessionKind::Scoped {
                pid: 1,
                argv: vec![],
            },
        );
        let chosen = resolve(None, false, false).unwrap();
        assert_eq!(chosen, sc_dir);
        let _ = amb_dir;
    }

    #[test]
    #[serial]
    fn include_ambient_picks_newest_regardless() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let _sc_dir = make_session(
            home.path(),
            SessionKind::Scoped {
                pid: 1,
                argv: vec![],
            },
        );
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let amb_dir = make_session(home.path(), SessionKind::Ambient);
        let chosen = resolve(None, false, true).unwrap();
        assert_eq!(chosen, amb_dir);
    }

    #[test]
    #[serial]
    fn id_match_overrides_kind_preference() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let amb_dir = make_session(home.path(), SessionKind::Ambient);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let _sc_dir = make_session(
            home.path(),
            SessionKind::Scoped {
                pid: 1,
                argv: vec![],
            },
        );
        let amb_name = amb_dir.file_name().unwrap().to_string_lossy().to_string();
        let short = &amb_name[amb_name.len() - 8..];
        let chosen = resolve(Some(short.to_string()), false, false).unwrap();
        assert_eq!(chosen, amb_dir);
    }

    /// #116: the id path must accept everything `resolve_session` accepts —
    /// including a `SessionMetadata.name` — not just directory substrings.
    #[test]
    #[serial]
    fn id_path_resolves_session_names() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let mut meta = SessionMetadata::now_starting(SessionId::new());
        meta.name = Some("ltx2-masterchouffe".into());
        let w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "x".into()).unwrap();

        let chosen = resolve(Some("ltx2-masterchouffe".into()), false, false).unwrap();
        assert_eq!(chosen, dir);
    }

    #[test]
    #[serial]
    fn no_scoped_falls_back_to_ambient() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let amb_dir = make_session(home.path(), SessionKind::Ambient);
        let chosen = resolve(None, false, false).unwrap();
        assert_eq!(chosen, amb_dir);
    }
}

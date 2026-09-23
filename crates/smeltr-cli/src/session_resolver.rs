//! Resolve a session directory from CLI arguments, preferring Scoped sessions
//! over the daemon's Ambient session by default.

use anyhow::{anyhow, Context, Result};
use smeltr_core::reader::read_metadata;
use smeltr_core::session::SessionKind;
use smeltr_core::session_resolve::{latest_session, sessions_newest_first};
use std::path::PathBuf;

/// Pick the session directory the user most likely means.
///
/// Resolution order ("newest" is by start time, never by directory name):
/// 1. If `id` is given, resolve it like every other session arg
///    (`smeltr_core::session_resolve::resolve_session`: short id, full UUID,
///    directory name or `SessionMetadata.name` — #116).
/// 2. Else if `prefer_post_mortem` is true (`analyze`), return the newest
///    post-mortem when nothing was recorded after it (#166).
/// 3. Else if `include_ambient` is true, return the newest session of any kind.
/// 4. Else return the newest recording, falling back to newest overall.
pub fn resolve(
    id: Option<String>,
    prefer_post_mortem: bool,
    include_ambient: bool,
) -> Result<PathBuf> {
    if let Some(id) = id {
        return smeltr_core::session_resolve::resolve_session(&id)
            .map_err(|e| anyhow!("could not resolve session {id:?}: {e}"));
    }
    let sessions = sessions_newest_first().context("listing sessions")?;
    if sessions.is_empty() {
        return Err(anyhow!("no sessions found under SMELTR_HOME"));
    }
    if prefer_post_mortem {
        let is_pm = |d: &PathBuf| {
            d.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("post-mortem-"))
                .unwrap_or(false)
        };
        let is_scoped = |d: &PathBuf| {
            read_metadata(d)
                .map(|m| matches!(m.kind, SessionKind::Scoped { .. }))
                .unwrap_or(false)
        };
        // #166: a stale post-mortem must not shadow a scoped session
        // recorded after it — prefer it only when it is still the most
        // recent thing that happened.
        if let Some(pm) = sessions
            .iter()
            .find(|d| is_pm(d) || is_scoped(d))
            .filter(|d| is_pm(d))
        {
            return Ok(pm.clone());
        }
    }
    if include_ambient {
        return Ok(sessions[0].clone());
    }
    latest_session().map_err(|e| anyhow!("{e}"))
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

#[cfg(test)]
mod post_mortem_recency_tests {
    use super::*;
    use serial_test::serial;
    use smeltr_core::session::{SessionId, SessionKind, SessionMetadata};
    use smeltr_core::writer::SessionWriter;

    fn make_session_at(started: &str, kind: SessionKind) -> PathBuf {
        let mut meta = SessionMetadata::now_starting(SessionId::new());
        meta.started_rfc3339 = started.to_string();
        meta.kind = kind;
        let w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "x".into()).unwrap();
        dir
    }

    fn into_post_mortem(dir: PathBuf, label: &str) -> PathBuf {
        let pm = dir.parent().unwrap().join(label);
        std::fs::rename(&dir, &pm).unwrap();
        pm
    }

    #[test]
    #[serial]
    fn stale_post_mortem_does_not_shadow_fresh_scoped() {
        // #166: a days-old crash post-mortem must not shadow the scoped
        // session the user just recorded.
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let pm_src = make_session_at("2026-07-01T00:00:00Z", SessionKind::Ambient);
        let _pm = into_post_mortem(
            pm_src,
            "post-mortem-crash-report-2026-07-01-000000-deadbeef",
        );
        let sc_dir = make_session_at(
            "2026-07-02T00:00:00Z",
            SessionKind::Scoped {
                pid: 1,
                argv: vec![],
            },
        );
        let chosen = resolve(None, true, false).unwrap();
        assert_eq!(chosen, sc_dir);
    }

    #[test]
    #[serial]
    fn newest_post_mortem_wins_whatever_its_label() {
        // #241: `mach-exception` sorts after `crash-report` by name; the
        // newer post-mortem must win anyway.
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let old = make_session_at("2026-07-01T00:00:00Z", SessionKind::Ambient);
        let _old = into_post_mortem(old, "post-mortem-mach-exception-2026-07-01-000000-aaaa0000");
        let new = make_session_at("2026-07-02T00:00:00Z", SessionKind::Ambient);
        let new = into_post_mortem(new, "post-mortem-crash-report-2026-07-02-000000-bbbb0000");
        assert_eq!(resolve(None, true, false).unwrap(), new);
    }

    #[test]
    #[serial]
    fn fresh_post_mortem_still_preferred_after_crash() {
        // The #153 crash flow: the post-mortem written seconds after the
        // crash postdates the crashed scoped session and must win.
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let _sc_dir = make_session_at(
            "2026-07-01T00:00:00Z",
            SessionKind::Scoped {
                pid: 1,
                argv: vec![],
            },
        );
        let pm_src = make_session_at("2026-07-01T00:05:00Z", SessionKind::Ambient);
        let pm = into_post_mortem(
            pm_src,
            "post-mortem-crash-report-2026-07-01-000500-deadbeef",
        );
        let chosen = resolve(None, true, false).unwrap();
        assert_eq!(chosen, pm);
    }
}

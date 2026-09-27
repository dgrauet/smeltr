//! `smeltr analyze` command.

use anyhow::Context;
use anyhow::Result;
use smeltr_core::reader::read_events;

pub fn run(arg_last: bool, session_id: Option<String>, include_ambient: bool) -> Result<()> {
    let dir = pick(arg_last, session_id, include_ambient)?;
    let report = build_report(&dir)?;
    println!("{}", report.render());
    Ok(())
}

/// The session to analyze. No argument means `--last` (#269: only the
/// flag preferred the newest post-mortem, so the two disagreed).
fn pick(
    arg_last: bool,
    session_id: Option<String>,
    include_ambient: bool,
) -> Result<std::path::PathBuf> {
    let last = arg_last || session_id.is_none();
    crate::session_resolver::resolve(session_id, last, include_ambient)
}

fn build_report(dir: &std::path::Path) -> Result<smeltr_analyzer::report::Report> {
    let events =
        read_events(dir).with_context(|| format!("reading events from {}", dir.display()))?;
    Ok(smeltr_analyzer::analyze_session(dir, &events))
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use smeltr_core::event::{Event, Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;

    /// #269: `smeltr analyze` with no argument is documented as "same as
    /// --last", but only `--last` preferred the post-mortem, so the two
    /// picked different sessions.
    #[test]
    #[serial]
    fn no_argument_picks_what_last_picks() {
        use smeltr_core::session::SessionKind;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let mut scoped = SessionMetadata::now_starting(SessionId::new());
        scoped.kind = SessionKind::Scoped {
            pid: 1,
            argv: vec!["python".into()],
        };
        scoped.started_rfc3339 = "2026-09-01T00:00:00Z".into();
        SessionWriter::create(scoped)
            .unwrap()
            .finalize(Some(0), "2026-09-01T00:01:00Z".into())
            .unwrap();
        // A post-mortem written after the recording.
        let pm = home
            .path()
            .join("sessions/post-mortem-crash-report-2026-09-02-000000-abcd1234");
        std::fs::create_dir_all(&pm).unwrap();
        let mut meta = SessionMetadata::now_starting(SessionId::new());
        meta.started_rfc3339 = "2026-09-02T00:00:00Z".into();
        smeltr_core::session::write_metadata(&pm, &meta).unwrap();
        std::fs::write(pm.join("events.cbor.zst"), b"").unwrap();

        let last = super::pick(true, None, false).unwrap();
        let bare = super::pick(false, None, false).unwrap();
        std::env::remove_var("SMELTR_HOME");
        assert_eq!(bare, last);
    }

    #[test]
    #[serial]
    fn report_header_uses_metadata_id_not_event_stamps() {
        // #170: post-mortem sessions carry events stamped with the ambient
        // session that ingested them; the header must name the session that
        // was actually analyzed (the directory's metadata).
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let meta_id = SessionId::new();
        let foreign_id = SessionId::new();
        let meta = SessionMetadata::now_starting(meta_id);
        let mut w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        w.write_event(&Event {
            ts_mono_ns: 1,
            ts_wall_ns: 1,
            session_id: foreign_id.0,
            source: Source::System,
            pid: None,
            seq: 1,
            payload: Payload::SessionStarted { wall_unix_ns: 1 },
        })
        .unwrap();
        w.finalize(Some(0), "x".into()).unwrap();

        let report = super::build_report(&dir).unwrap();
        assert_eq!(
            report.session_short.as_deref(),
            Some(meta_id.short().as_str()),
        );
    }
}

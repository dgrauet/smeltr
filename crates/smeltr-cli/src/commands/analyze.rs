//! `smeltr analyze` command.

use anyhow::Context;
use anyhow::Result;
use smeltr_core::reader::read_events_checked;

pub fn run(arg_last: bool, session_id: Option<String>, include_ambient: bool) -> Result<()> {
    let dir = crate::session_resolver::resolve(session_id, arg_last, include_ambient)?;
    let report = build_report(&dir)?;
    println!("{}", report.render());
    Ok(())
}

fn build_report(dir: &std::path::Path) -> Result<smeltr_analyzer::report::Report> {
    let (events, damage) = read_events_checked(dir)
        .with_context(|| format!("reading events from {}", dir.display()))?;
    Ok(smeltr_analyzer::analyze_session_checked(
        dir,
        &events,
        damage.as_ref(),
    ))
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use smeltr_core::event::{Event, Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;

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

    /// #268: a truncated session is reported as incomplete, not analyzed as
    /// if the readable part were the whole run ("No instrumented GPU
    /// workload" from a file cut at 30 %).
    #[test]
    #[serial]
    fn truncated_session_is_reported_incomplete() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let mut w = SessionWriter::create(SessionMetadata::now_starting(SessionId::new())).unwrap();
        let dir = w.dir().to_path_buf();
        for i in 0..200u64 {
            w.write_event(&Event {
                ts_mono_ns: i,
                ts_wall_ns: i,
                session_id: uuid::Uuid::nil(),
                source: Source::System,
                pid: None,
                seq: i,
                payload: Payload::SessionStarted { wall_unix_ns: i },
            })
            .unwrap();
            if i % 20 == 19 {
                w.flush().unwrap();
            }
        }
        w.finalize(Some(0), "x".into()).unwrap();
        let path = dir.join("events.cbor.zst");
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() * 3 / 10]).unwrap();

        let report = super::build_report(&dir).unwrap();
        let titles: Vec<&str> = report.findings.iter().map(|f| f.title.as_str()).collect();
        assert!(
            titles[0].starts_with("Session data is incomplete"),
            "first finding must flag the damage: {titles:?}"
        );
        assert!(
            !titles
                .iter()
                .any(|t| t.starts_with("No instrumented GPU workload")),
            "no conclusion from missing data: {titles:?}"
        );
        assert!(report.render().contains("Session data is incomplete"));
    }
}

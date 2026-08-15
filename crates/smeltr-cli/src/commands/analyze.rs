//! `smeltr analyze` command.

use anyhow::Context;
use anyhow::Result;
use smeltr_analyzer::analyze;
use smeltr_core::reader::{read_events, read_metadata};

pub fn run(arg_last: bool, session_id: Option<String>, include_ambient: bool) -> Result<()> {
    let dir = crate::session_resolver::resolve(session_id, arg_last, include_ambient)?;
    let report = build_report(&dir)?;
    println!("{}", report.render());
    Ok(())
}

fn build_report(dir: &std::path::Path) -> Result<smeltr_analyzer::report::Report> {
    let events =
        read_events(dir).with_context(|| format!("reading events from {}", dir.display()))?;
    let mut report = analyze(&events);

    if let Ok(meta) = read_metadata(dir) {
        // #170: post-mortem sessions carry events stamped with the ambient
        // session that ingested them — name the session actually analyzed.
        report.session_short = Some(meta.session_id.short());
    }

    // Les deux jointures rétroactives (#153, #200) vivent dans l'analyzer et
    // sont appelées à l'identique par le MCP : les avoir ici seulement, c'est
    // ce qui privait `get_session_summary` du verdict de crash (#204).
    smeltr_analyzer::crash_join::join_crash(&mut report, dir);
    smeltr_analyzer::crash_join::join_jetsam(&mut report, dir);

    Ok(report)
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
}

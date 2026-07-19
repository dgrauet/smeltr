//! `smeltr analyze` command.

use anyhow::Context;
use anyhow::Result;
use smeltr_analyzer::analyze;
use smeltr_analyzer::crash_join::{crash_finding, find_crash_report, CRASH_REPORT_GRACE_NS};
use smeltr_core::reader::{read_events, read_metadata};
use smeltr_core::session::SessionKind;
use std::path::PathBuf;

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

    // #153: ReportCrash writes the .ips seconds AFTER the crashed child
    // dies, when the scoped session is already finalized — the live probe
    // cannot land it there. Join it back at analyze time instead; also
    // works on sessions recorded before this feature existed.
    //
    // The window uses the metadata's RFC3339 timestamps (real wall clock
    // at write time), NOT the events' ts_wall_ns: those are derived from
    // the monotonic clock, which stops during system sleep — a run that
    // slept mid-recording has event wall times behind reality by the
    // whole sleep duration.
    if let Ok(meta) = read_metadata(dir) {
        // #170: post-mortem sessions carry events stamped with the ambient
        // session that ingested them — name the session actually analyzed.
        report.session_short = Some(meta.session_id.short());
        if let SessionKind::Scoped { pid, .. } = &meta.kind {
            if meta.exit_code != Some(0) {
                if let (Some(start_ns), Some(end_ns), Some(reports_dir)) = (
                    rfc3339_unix_ns(&meta.started_rfc3339),
                    meta.ended_rfc3339.as_deref().and_then(rfc3339_unix_ns),
                    diagnostic_reports_dir(),
                ) {
                    if let Some(join) = find_crash_report(
                        &reports_dir,
                        *pid,
                        start_ns,
                        end_ns,
                        CRASH_REPORT_GRACE_NS,
                    ) {
                        report.findings.insert(0, crash_finding(&join));
                    }
                }
            }
        }
    }

    Ok(report)
}

fn rfc3339_unix_ns(s: &str) -> Option<u64> {
    use time::format_description::well_known::Rfc3339;
    let t = time::OffsetDateTime::parse(s, &Rfc3339).ok()?;
    u64::try_from(t.unix_timestamp_nanos()).ok()
}

/// `~/Library/Logs/DiagnosticReports`, overridable for tests via
/// `SMELTR_DIAGNOSTIC_REPORTS_DIR`.
fn diagnostic_reports_dir() -> Option<PathBuf> {
    if let Some(over) = std::env::var_os("SMELTR_DIAGNOSTIC_REPORTS_DIR") {
        return Some(PathBuf::from(over));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Logs/DiagnosticReports"))
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

//! `smeltr export` subcommand: dump a session to chrome-trace or raw JSON.

use anyhow::{anyhow, Context};
use smeltr_analyzer::export::{to_chrome_trace, to_json_raw};
use smeltr_core::reader::{read_events, read_metadata};
use smeltr_mcp::types::resolve_session;
use std::io::Write;
use std::path::PathBuf;

pub fn run(session: &str, format: &str, output: Option<&str>) -> anyhow::Result<()> {
    let dir = resolve_session(session)
        .map_err(|e| anyhow!("could not resolve session {session:?}: {e}"))?;
    let meta = read_metadata(&dir).context("read session metadata")?;
    let events = read_events(&dir).context("read session events")?;

    let bytes = match format {
        "chrome-trace" => to_chrome_trace(&events, &meta),
        "json" => to_json_raw(&events, &meta),
        other => {
            return Err(anyhow!(
                "unknown --format {other:?}; supported: chrome-trace, json"
            ));
        }
    };

    let target: Option<PathBuf> = match output {
        Some("-") => None,
        Some(p) => Some(PathBuf::from(p)),
        None => {
            let short = meta.session_id.short();
            Some(PathBuf::from(format!("{short}.json")))
        }
    };

    match target {
        Some(path) => {
            std::fs::write(&path, &bytes).with_context(|| format!("write {}", path.display()))?;
            eprintln!("smeltr: wrote {} ({} bytes)", path.display(), bytes.len());
        }
        None => {
            let mut out = std::io::stdout().lock();
            out.write_all(bytes.as_bytes()).context("write stdout")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use smeltr_core::event::{Event, Payload, Source};
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    fn make_session_with_one_mark() -> SessionId {
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        for i in 0..25u64 {
            w.write_event(&Event {
                ts_mono_ns: i * 1000,
                ts_wall_ns: i * 1000,
                session_id: Uuid::nil(),
                source: Source::Mark,
                pid: None,
                seq: i,
                payload: Payload::Mark {
                    label: format!("m{i}"),
                },
            })
            .unwrap();
        }
        w.finalize(Some(0), "ok".into()).unwrap();
        id
    }

    #[test]
    #[serial_test::serial]
    fn export_writes_chrome_trace_file() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = make_session_with_one_mark();
        let out = home.path().join("trace.json");

        super::run(&id.short(), "chrome-trace", Some(out.to_str().unwrap())).unwrap();

        assert!(out.exists());
        let s = std::fs::read_to_string(&out).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert!(v["traceEvents"].is_array());
        assert_eq!(v["displayTimeUnit"], "ms");
    }

    #[test]
    #[serial_test::serial]
    fn export_writes_json_raw_file() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = make_session_with_one_mark();
        let out = home.path().join("raw.json");

        super::run(&id.short(), "json", Some(out.to_str().unwrap())).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert!(v["events"].is_array());
        assert!(v["metadata"].is_object());
    }

    #[test]
    #[serial_test::serial]
    fn export_rejects_unknown_format() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = make_session_with_one_mark();
        let out = home.path().join("trace.json");

        let err = super::run(&id.short(), "bogus", Some(out.to_str().unwrap())).unwrap_err();
        assert!(err.to_string().contains("unknown --format"));
    }

    #[test]
    #[serial_test::serial]
    fn export_resolves_by_session_name() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::set_var("SMELTR_SESSION_NAME", "ltx2-baseline");
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        for i in 0..25u64 {
            w.write_event(&Event {
                ts_mono_ns: i * 1000,
                ts_wall_ns: i * 1000,
                session_id: Uuid::nil(),
                source: Source::Mark,
                pid: None,
                seq: i,
                payload: Payload::Mark {
                    label: format!("m{i}"),
                },
            })
            .unwrap();
        }
        w.finalize(Some(0), "ok".into()).unwrap();
        std::env::remove_var("SMELTR_SESSION_NAME");

        let out = home.path().join("by-name.json");
        super::run("ltx2-baseline", "chrome-trace", Some(out.to_str().unwrap())).unwrap();
        assert!(out.exists());
    }
}

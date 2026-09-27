//! `smeltr export` subcommand: dump a session to chrome-trace or raw JSON.

use crate::session_resolver::resolve_arg;
use anyhow::{anyhow, Context};
use smeltr_analyzer::export::{to_chrome_trace, to_json_raw};
use smeltr_core::reader::{read_events, read_metadata};
use std::io::Write;
use std::path::PathBuf;

pub fn run(
    session: Option<&str>,
    last: bool,
    format: &str,
    output: Option<&str>,
    force: bool,
) -> anyhow::Result<()> {
    let dir = resolve_arg(session, last)?;
    // Checked before anything is decoded (#287).
    let target: Option<PathBuf> = match output {
        Some("-") => None,
        Some(p) => Some(PathBuf::from(p)),
        None => {
            let short = read_metadata(&dir)
                .context("read session metadata")?
                .session_id
                .short();
            Some(PathBuf::from(format!("{short}.json")))
        }
    }
    .map(|p| {
        smeltr_core::session::checked_export_target(&p, force).map_err(|e| {
            anyhow!(e.replace(
                "allow replacing it explicitly",
                "pass --force to replace it"
            ))
        })
    })
    .transpose()?;
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

    match target {
        Some(path) => {
            // `create_new` unless --force: nothing can appear at the path
            // between the check above and this write.
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .create_new(!force)
                .truncate(true)
                .open(&path)
                .with_context(|| format!("open {}", path.display()))?;
            f.write_all(bytes.as_bytes())
                .with_context(|| format!("write {}", path.display()))?;
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

    fn export_to(id: &SessionId, out: &std::path::Path, force: bool) -> anyhow::Result<()> {
        super::run(
            Some(&id.short()),
            false,
            "chrome-trace",
            Some(out.to_str().unwrap()),
            force,
        )
    }

    /// #287: the CLI had the unguarded path the MCP tool lost in #271 — the
    /// one that destroyed a real session: exporting onto a session's own
    /// event file replaced it with chrome-trace JSON.
    #[test]
    #[serial_test::serial]
    fn export_never_writes_into_the_sessions_store() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = make_session_with_one_mark();
        let dir = smeltr_core::reader::find_session_dir(id).unwrap().unwrap();
        let events = smeltr_core::session::events_path_for_read(&dir);
        let before = std::fs::read(&events).unwrap();

        assert!(export_to(&id, &events, true).is_err(), "even with --force");
        assert!(export_to(&id, &dir.join("trace.json"), false).is_err());
        let link = home.path().join("link.json");
        std::os::unix::fs::symlink(&events, &link).unwrap();
        assert!(export_to(&id, &link, true).is_err(), "through a symlink");

        assert_eq!(
            std::fs::read(&events).unwrap(),
            before,
            "session file changed"
        );
    }

    /// #287: an existing file was replaced silently.
    #[test]
    #[serial_test::serial]
    fn export_replaces_an_existing_file_only_with_force() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_NAME");
        let id = make_session_with_one_mark();
        let out = home.path().join("mine.json");
        std::fs::write(&out, b"keep me").unwrap();
        assert!(export_to(&id, &out, false).is_err());
        assert_eq!(std::fs::read(&out).unwrap(), b"keep me");
        export_to(&id, &out, true).unwrap();
        assert!(std::fs::read_to_string(&out)
            .unwrap()
            .contains("traceEvents"));
    }

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
                    fields: Default::default(),
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

        super::run(
            Some(&id.short()),
            false,
            "chrome-trace",
            Some(out.to_str().unwrap()),
            false,
        )
        .unwrap();

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

        super::run(
            Some(&id.short()),
            false,
            "json",
            Some(out.to_str().unwrap()),
            false,
        )
        .unwrap();

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

        let err = super::run(
            Some(&id.short()),
            false,
            "bogus",
            Some(out.to_str().unwrap()),
            false,
        )
        .unwrap_err();
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
                    fields: Default::default(),
                },
            })
            .unwrap();
        }
        w.finalize(Some(0), "ok".into()).unwrap();
        std::env::remove_var("SMELTR_SESSION_NAME");

        let out = home.path().join("by-name.json");
        super::run(
            Some("ltx2-baseline"),
            false,
            "chrome-trace",
            Some(out.to_str().unwrap()),
            false,
        )
        .unwrap();
        assert!(out.exists());
    }
}

//! Append-only session writer. One instance per active session.

use crate::codec::write_frame;
use crate::event::Event;
use crate::session::{events_path, metadata_path, session_dir, SessionMetadata};
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;

pub struct SessionWriter {
    dir: PathBuf,
    events: BufWriter<File>,
    metadata: SessionMetadata,
}

impl SessionWriter {
    pub fn create(metadata: SessionMetadata) -> std::io::Result<Self> {
        let dir = session_dir(&metadata);
        create_dir_all(&dir)?;
        let events = OpenOptions::new()
            .create(true)
            .append(true)
            .open(events_path(&dir))?;
        let w = Self {
            dir,
            events: BufWriter::new(events),
            metadata,
        };
        w.persist_metadata()?;
        Ok(w)
    }

    pub fn write_event(&mut self, ev: &Event) -> Result<(), crate::codec::CodecError> {
        write_frame(&mut self.events, ev)?;
        Ok(())
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    pub fn finalize(
        mut self,
        exit_code: Option<i32>,
        ended_rfc3339: String,
    ) -> std::io::Result<()> {
        self.events.flush()?;
        self.metadata.exit_code = exit_code;
        self.metadata.ended_rfc3339 = Some(ended_rfc3339);
        self.persist_metadata()
    }

    fn persist_metadata(&self) -> std::io::Result<()> {
        let toml = toml_simple::to_string(&self.metadata);
        std::fs::write(metadata_path(&self.dir), toml)
    }
}

/// Tiny TOML emitter so we avoid pulling a full TOML crate for ~6 fields.
mod toml_simple {
    use crate::session::SessionMetadata;
    pub fn to_string(m: &SessionMetadata) -> String {
        let mut s = String::new();
        s.push_str(&format!("session_id = \"{}\"\n", m.session_id));
        s.push_str(&format!("started_rfc3339 = \"{}\"\n", m.started_rfc3339));
        if let Some(end) = &m.ended_rfc3339 {
            s.push_str(&format!("ended_rfc3339 = \"{}\"\n", esc(end)));
        }
        s.push_str(&format!("host = \"{}\"\n", esc(&m.host)));
        if let Some(v) = &m.mlx_version {
            s.push_str(&format!("mlx_version = \"{}\"\n", esc(v)));
        }
        if let Some(c) = m.exit_code {
            s.push_str(&format!("exit_code = {c}\n"));
        }
        let args: Vec<String> = m.argv.iter().map(|a| format!("\"{}\"", esc(a))).collect();
        s.push_str(&format!("argv = [{}]\n", args.join(", ")));
        s
    }
    fn esc(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Payload, Source};
    use crate::session::SessionId;
    use uuid::Uuid;

    fn temp_home() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", d.path());
        d
    }

    #[test]
    fn writer_creates_dir_and_metadata() {
        let _home = temp_home();
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let w = SessionWriter::create(meta.clone()).unwrap();
        assert!(w.dir().join("metadata.toml").exists());
        assert!(w.dir().join("events.cbor").exists());
    }

    #[test]
    fn writer_appends_events() {
        let _home = temp_home();
        let meta = SessionMetadata::now_starting(SessionId::new());
        let mut w = SessionWriter::create(meta).unwrap();
        for i in 0..3 {
            w.write_event(&Event {
                ts_mono_ns: i,
                ts_wall_ns: 0,
                session_id: Uuid::nil(),
                source: Source::Mark,
                pid: None,
                seq: i,
                payload: Payload::Mark {
                    label: format!("mk-{i}"),
                },
            })
            .unwrap();
        }
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "2026-05-13T12:00:00Z".into()).unwrap();
        let meta_str = std::fs::read_to_string(dir.join("metadata.toml")).unwrap();
        assert!(meta_str.contains("exit_code = 0"));
        let events_size = std::fs::metadata(dir.join("events.cbor")).unwrap().len();
        assert!(events_size > 30, "size {events_size}");
    }
}

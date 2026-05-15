//! Append-only session writer. One instance per active session.

use crate::codec::write_frame;
use crate::event::Event;
use crate::session::{events_path_zst, metadata_path, session_dir, SessionMetadata};
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

pub struct SessionWriter {
    dir: PathBuf,
    encoder: Option<zstd::stream::Encoder<'static, File>>,
    metadata: SessionMetadata,
}

impl SessionWriter {
    pub fn create(metadata: SessionMetadata) -> std::io::Result<Self> {
        let dir = session_dir(&metadata);
        create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(events_path_zst(&dir))?;
        let encoder = zstd::stream::Encoder::new(file, 3)?;
        let w = Self {
            dir,
            encoder: Some(encoder),
            metadata,
        };
        w.persist_metadata()?;
        Ok(w)
    }

    pub fn write_event(&mut self, ev: &Event) -> Result<(), crate::codec::CodecError> {
        let enc = self.encoder.as_mut().ok_or_else(|| {
            crate::codec::CodecError::Io(std::io::Error::other("writer already finalized"))
        })?;
        write_frame(enc, ev)?;
        Ok(())
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Flushes the underlying writer so events become visible to readers.
    /// Zstd streaming is friendly to mid-stream flush — readers tolerate
    /// truncated trailing frames.
    pub fn flush(&mut self) -> std::io::Result<()> {
        if let Some(enc) = self.encoder.as_mut() {
            enc.flush()?;
        }
        Ok(())
    }

    pub fn finalize(
        mut self,
        exit_code: Option<i32>,
        ended_rfc3339: String,
    ) -> std::io::Result<()> {
        if let Some(enc) = self.encoder.take() {
            let _file = enc.finish()?;
        }
        self.metadata.exit_code = exit_code;
        self.metadata.ended_rfc3339 = Some(ended_rfc3339);
        self.persist_metadata()
    }

    fn persist_metadata(&self) -> std::io::Result<()> {
        let text = toml::to_string(&self.metadata)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        std::fs::write(metadata_path(&self.dir), text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Payload, Source};
    use crate::session::SessionId;
    use serial_test::serial;
    use uuid::Uuid;

    fn temp_home() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", d.path());
        d
    }

    #[test]
    #[serial]
    fn writer_creates_dir_and_metadata() {
        let _home = temp_home();
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let w = SessionWriter::create(meta.clone()).unwrap();
        assert!(w.dir().join("metadata.toml").exists());
        assert!(w.dir().join("events.cbor.zst").exists());
    }

    #[test]
    #[serial]
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
        let events_size = std::fs::metadata(dir.join("events.cbor.zst"))
            .unwrap()
            .len();
        assert!(events_size > 10, "size {events_size}");
    }

    #[test]
    #[serial]
    fn flush_makes_events_visible_to_reader() {
        let _home = temp_home();
        let meta = SessionMetadata::now_starting(SessionId::new());
        let mut w = SessionWriter::create(meta).unwrap();
        w.write_event(&Event {
            ts_mono_ns: 1,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: 1,
            payload: Payload::Mark { label: "hi".into() },
        })
        .unwrap();
        let dir = w.dir().to_path_buf();
        w.flush().unwrap();

        let events = crate::reader::read_events(&dir).unwrap();
        assert_eq!(events.len(), 1, "expected 1 event after flush");

        w.finalize(Some(0), "2026-05-14T00:00:00Z".into()).unwrap();
    }

    #[test]
    #[serial]
    fn writer_creates_zstd_events_file() {
        let _home = temp_home();
        let meta = SessionMetadata::now_starting(SessionId::new());
        let w = SessionWriter::create(meta.clone()).unwrap();
        let dir = w.dir().to_path_buf();
        drop(w);
        assert!(
            dir.join("events.cbor.zst").exists(),
            "expected events.cbor.zst, dir={:?}",
            dir
        );
        assert!(
            !dir.join("events.cbor").exists(),
            "legacy .cbor should not be created for new sessions"
        );
    }

    #[test]
    #[serial]
    fn write_then_read_back_compressed() {
        let _home = temp_home();
        let meta = SessionMetadata::now_starting(SessionId::new());
        let mut w = SessionWriter::create(meta).unwrap();
        for i in 0..50 {
            w.write_event(&Event {
                ts_mono_ns: i,
                ts_wall_ns: 0,
                session_id: Uuid::nil(),
                source: Source::Mark,
                pid: None,
                seq: i,
                payload: Payload::Mark {
                    label: format!("compressible-{i}"),
                },
            })
            .unwrap();
        }
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "2026-05-14T00:00:00Z".into()).unwrap();

        let events = crate::reader::read_events(&dir).unwrap();
        assert_eq!(events.len(), 50);
        assert_eq!(events[49].seq, 49);

        let raw = std::fs::metadata(dir.join("events.cbor.zst"))
            .unwrap()
            .len();
        assert!(
            raw < 1200,
            "compressed size {raw} too large for 50 redundant events"
        );
    }
}

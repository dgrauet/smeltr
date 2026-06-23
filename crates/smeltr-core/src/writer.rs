//! Append-only session writer. One instance per active session.

use crate::chunked::{self, ChunkConfig, ChunkIndexEntry};
use crate::codec::write_frame;
use crate::event::Event;
use crate::session::{events_path_zst, metadata_path, session_dir, SessionMetadata};
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

enum Backend {
    Legacy(Option<zstd::stream::Encoder<'static, File>>),
    Chunked(Option<ChunkedState>),
}

struct ChunkedState {
    file: File,
    cfg: ChunkConfig,
    cursor: u64,
    enc: zstd::stream::Encoder<'static, Vec<u8>>,
    event_count: u32,
    uncompressed_bytes: u64,
    min_ts: u64,
    max_ts: u64,
    source_bitmap: u64,
    index: Vec<ChunkIndexEntry>,
    poisoned: bool,
}

pub struct SessionWriter {
    dir: PathBuf,
    backend: Backend,
    metadata: SessionMetadata,
}

impl SessionWriter {
    pub fn create(metadata: SessionMetadata) -> std::io::Result<Self> {
        // Check env var opt-in for chunked mode.
        if std::env::var("SMELTR_SESSION_INDEX").as_deref() == Ok("1") {
            return Self::create_with_chunk_config(metadata, Some(ChunkConfig::default()));
        }
        // Legacy: append-mode streaming zstd directly to file.
        let dir = session_dir(&metadata);
        create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(events_path_zst(&dir))?;
        let encoder = zstd::stream::Encoder::new(file, 3)?;
        let w = Self {
            dir,
            backend: Backend::Legacy(Some(encoder)),
            metadata,
        };
        w.persist_metadata()?;
        Ok(w)
    }

    pub fn create_with_chunk_config(
        metadata: SessionMetadata,
        cfg: Option<ChunkConfig>,
    ) -> std::io::Result<Self> {
        let Some(cfg) = cfg else {
            return Self::create_legacy(metadata);
        };
        let dir = session_dir(&metadata);
        create_dir_all(&dir)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(events_path_zst(&dir))?;
        file.write_all(&chunked::HEAD_MAGIC)?;
        let enc = zstd::stream::Encoder::new(Vec::new(), 3)?;
        let state = ChunkedState {
            file,
            cfg,
            cursor: chunked::HEAD_MAGIC.len() as u64,
            enc,
            event_count: 0,
            uncompressed_bytes: 0,
            min_ts: u64::MAX,
            max_ts: 0,
            source_bitmap: 0,
            index: Vec::new(),
            poisoned: false,
        };
        let w = Self {
            dir,
            backend: Backend::Chunked(Some(state)),
            metadata,
        };
        w.persist_metadata()?;
        Ok(w)
    }

    /// Internal: create a legacy writer (used when cfg is None).
    fn create_legacy(metadata: SessionMetadata) -> std::io::Result<Self> {
        let dir = session_dir(&metadata);
        create_dir_all(&dir)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(events_path_zst(&dir))?;
        let encoder = zstd::stream::Encoder::new(file, 3)?;
        let w = Self {
            dir,
            backend: Backend::Legacy(Some(encoder)),
            metadata,
        };
        w.persist_metadata()?;
        Ok(w)
    }

    pub fn write_event(&mut self, ev: &Event) -> Result<(), crate::codec::CodecError> {
        match &mut self.backend {
            Backend::Legacy(Some(enc)) => {
                write_frame(enc, ev)?;
                return Ok(());
            }
            Backend::Legacy(None) => {
                return Err(crate::codec::CodecError::Io(std::io::Error::other(
                    "writer already finalized",
                )));
            }
            Backend::Chunked(None) => {
                return Err(crate::codec::CodecError::Io(std::io::Error::other(
                    "writer already finalized",
                )));
            }
            Backend::Chunked(Some(ref mut st)) => {
                if st.poisoned {
                    return Err(crate::codec::CodecError::Io(std::io::Error::other(
                        "writer poisoned",
                    )));
                }
                let n = write_frame(&mut st.enc, ev).inspect_err(|_| {
                    st.poisoned = true;
                })?;
                st.uncompressed_bytes += n as u64;
                st.event_count += 1;
                st.min_ts = st.min_ts.min(ev.ts_mono_ns);
                st.max_ts = st.max_ts.max(ev.ts_mono_ns);
                st.source_bitmap |= 1 << ev.source.as_u8();
            }
        };
        // After Chunked(Some) updates state, seal if thresholds are met.
        // Re-check via helper to release the borrow before calling self.seal().
        self.seal_if_needed()?;
        Ok(())
    }

    /// Seal if thresholds exceeded. Called after write_event updates state.
    fn seal_if_needed(&mut self) -> std::io::Result<()> {
        let should_seal = if let Backend::Chunked(Some(ref st)) = self.backend {
            !st.poisoned
                && (st.event_count >= st.cfg.max_events
                    || st.uncompressed_bytes >= st.cfg.max_bytes)
        } else {
            false
        };
        if should_seal {
            self.seal()?;
        }
        Ok(())
    }

    /// Seal the current in-progress chunk: finish zstd, write to file, append index entry.
    fn seal(&mut self) -> std::io::Result<()> {
        let Backend::Chunked(Some(ref mut st)) = self.backend else {
            return Ok(());
        };
        if st.event_count == 0 {
            return Ok(());
        }
        // Replace encoder with a fresh one.
        let old_enc = std::mem::replace(
            &mut st.enc,
            zstd::stream::Encoder::new(Vec::new(), 3).inspect_err(|_| {
                st.poisoned = true;
            })?,
        );
        let bytes = old_enc.finish().inspect_err(|_| {
            st.poisoned = true;
        })?;
        if bytes.len() > u32::MAX as usize {
            st.poisoned = true;
            return Err(std::io::Error::other("chunk too large"));
        }
        let offset_before = st.cursor;
        let comp_len = bytes.len() as u32;
        let mut buf = Vec::with_capacity(4 + bytes.len());
        buf.extend_from_slice(&comp_len.to_le_bytes());
        buf.extend_from_slice(&bytes);
        st.file.write_all(&buf).inspect_err(|_| {
            st.poisoned = true;
        })?;
        st.index.push(ChunkIndexEntry {
            offset: offset_before,
            comp_len,
            min_ts: st.min_ts,
            max_ts: st.max_ts,
            source_bitmap: st.source_bitmap,
            event_count: st.event_count,
        });
        st.cursor += buf.len() as u64;
        st.event_count = 0;
        st.uncompressed_bytes = 0;
        st.min_ts = u64::MAX;
        st.max_ts = 0;
        st.source_bitmap = 0;
        Ok(())
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Flushes the underlying writer so events become visible to readers.
    pub fn flush(&mut self) -> std::io::Result<()> {
        // For chunked: seal if threshold met, then flush file.
        if let Backend::Chunked(Some(ref st)) = self.backend {
            let should_seal = !st.poisoned
                && st.event_count > 0
                && st.uncompressed_bytes >= st.cfg.flush_min_bytes;
            if should_seal {
                self.seal()?;
            }
        }
        match &mut self.backend {
            Backend::Legacy(Some(enc)) => enc.flush(),
            Backend::Legacy(None) => Ok(()),
            Backend::Chunked(Some(st)) => st.file.flush(),
            Backend::Chunked(None) => Ok(()),
        }
    }

    pub fn finalize(
        mut self,
        exit_code: Option<i32>,
        ended_rfc3339: String,
    ) -> std::io::Result<()> {
        match &mut self.backend {
            Backend::Legacy(_) => {
                if let Backend::Legacy(Some(enc)) =
                    std::mem::replace(&mut self.backend, Backend::Legacy(None))
                {
                    enc.finish()?;
                }
            }
            Backend::Chunked(_) => {
                // Seal remaining buffered events.
                self.seal()?;
                if let Backend::Chunked(Some(mut st)) =
                    std::mem::replace(&mut self.backend, Backend::Chunked(None))
                {
                    if !st.poisoned {
                        if st.index.len() > chunked::MAX_CHUNKS {
                            tracing::warn!(
                                "chunked session has {} chunks, exceeds MAX_CHUNKS={}",
                                st.index.len(),
                                chunked::MAX_CHUNKS
                            );
                        }
                        chunked::write_footer_at(&mut st.file, st.cursor, &st.index)?;
                        st.file.flush()?;
                    }
                    // If poisoned: skip footer; file remains scan-recoverable.
                }
            }
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

    fn ev(ts: u64, src: Source) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source: src,
            pid: None,
            seq: ts,
            payload: Payload::Mark {
                label: format!("m-{ts}"),
                fields: Default::default(),
            },
        }
    }

    #[test]
    #[serial]
    fn chunked_writer_seals_by_event_count_and_finalizes_footer() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::set_var("SMELTR_SESSION_INDEX", "1");
        let meta = SessionMetadata::now_starting(SessionId::new());
        let dir = crate::session::session_dir(&meta);
        let cfg = crate::chunked::ChunkConfig {
            max_events: 4,
            max_bytes: 1 << 30,
            flush_min_bytes: 1 << 30,
        };
        let mut w = SessionWriter::create_with_chunk_config(meta, Some(cfg)).unwrap();
        for i in 0..10u64 {
            w.write_event(&ev(i, Source::Mark)).unwrap();
        }
        w.finalize(Some(0), "end".into()).unwrap();
        let mut f = std::fs::File::open(crate::session::events_path_zst(&dir)).unwrap();
        assert!(matches!(
            crate::chunked::detect(&mut f).unwrap(),
            crate::chunked::Format::Chunked
        ));
        let entries = crate::chunked::read_footer(&mut f)
            .unwrap()
            .expect("sealed footer");
        assert_eq!(entries.len(), 3); // 4 + 4 + 2
        assert_eq!(entries[0].event_count, 4);
        assert_eq!(entries[2].event_count, 2);
        assert_eq!(entries[0].offset, 4); // right after HEAD_MAGIC
        assert!(entries[0].source_bitmap & (1 << Source::Mark.as_u8()) != 0);
        std::env::remove_var("SMELTR_SESSION_INDEX");
    }

    #[test]
    #[serial]
    fn chunked_flush_seals_so_reader_sees_events_before_finalize() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let meta = SessionMetadata::now_starting(SessionId::new());
        let dir = crate::session::session_dir(&meta);
        // flush_min_bytes=0 → any flush seals
        let cfg = crate::chunked::ChunkConfig {
            max_events: 1000,
            max_bytes: 1 << 30,
            flush_min_bytes: 0,
        };
        let mut w = SessionWriter::create_with_chunk_config(meta, Some(cfg)).unwrap();
        for i in 0..3u64 {
            w.write_event(&ev(i, Source::Mark)).unwrap();
        }
        w.flush().unwrap();
        // not finalized: footer absent, but scan recovers the sealed chunk
        let mut f = std::fs::File::open(crate::session::events_path_zst(&dir)).unwrap();
        assert!(crate::chunked::read_footer(&mut f).unwrap().is_none());
        let mut f = std::fs::File::open(crate::session::events_path_zst(&dir)).unwrap();
        assert_eq!(crate::chunked::scan_chunks(&mut f).unwrap().len(), 3);
    }

    #[test]
    #[serial]
    fn legacy_mode_unchanged_without_env() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_INDEX");
        let meta = SessionMetadata::now_starting(SessionId::new());
        let dir = crate::session::session_dir(&meta);
        let mut w = SessionWriter::create(meta).unwrap();
        w.write_event(&ev(1, Source::Mark)).unwrap();
        w.finalize(Some(0), "end".into()).unwrap();
        let mut f = std::fs::File::open(crate::session::events_path_zst(&dir)).unwrap();
        assert!(matches!(
            crate::chunked::detect(&mut f).unwrap(),
            crate::chunked::Format::Legacy
        ));
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
                    fields: Default::default(),
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
            payload: Payload::Mark {
                label: "hi".into(),
                fields: Default::default(),
            },
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
                    fields: Default::default(),
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

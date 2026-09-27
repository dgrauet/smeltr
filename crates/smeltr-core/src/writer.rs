//! Append-only session writer. One instance per active session.
//!
//! Both formats compress into memory and append to the events file in whole
//! units: one zstd frame (legacy) or one length-prefixed chunk (chunked).
//! A unit is on disk entirely or not at all. When a write fails (ENOSPC,
//! EIO), the file is cut back to the end of the last complete unit and the
//! unit stays pending, retried on the next seal, flush or finalize — so the
//! file is always a readable prefix of the session, and a disk that frees up
//! again loses nothing (#268).
//!
//! The legacy writer used to stream straight into a zstd encoder over the
//! file: an ENOSPC between an event's length prefix and its body left a hole
//! in the decompressed stream, and every event after it decoded to garbage.
//! The chunked writer poisoned itself for good on the first failed seal.

use crate::chunked::{self, ChunkConfig, ChunkIndexEntry};
use crate::codec::{encode_frame, CodecError, MAX_FRAME_BYTES};
use crate::event::Event;
use crate::session::{events_path_zst, session_dir, SessionMetadata};
use std::collections::VecDeque;
use std::fs::{create_dir_all, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// A legacy zstd frame ends once it holds this many uncompressed bytes, even
/// without a flush: it bounds the memory held and what a torn tail can cost.
pub const LEGACY_FRAME_BYTES: u64 = 256 * 1024;

/// Compressed bytes allowed to wait in memory while the disk refuses writes.
/// Beyond it, newly sealed units are dropped and counted in
/// `SessionMetadata::dropped_events`.
pub const MAX_PENDING_BYTES: usize = 64 * 1024 * 1024;

const ZSTD_LEVEL: i32 = 3;

fn new_encoder() -> io::Result<zstd::stream::Encoder<'static, Vec<u8>>> {
    zstd::stream::Encoder::new(Vec::new(), ZSTD_LEVEL)
}

/// Events compressed in memory, not yet sealed into a unit.
struct Batch {
    enc: zstd::stream::Encoder<'static, Vec<u8>>,
    event_count: u32,
    uncompressed_bytes: u64,
    min_ts: u64,
    max_ts: u64,
    source_bitmap: u64,
}

impl Batch {
    fn new() -> io::Result<Self> {
        Ok(Self {
            enc: new_encoder()?,
            event_count: 0,
            uncompressed_bytes: 0,
            min_ts: u64::MAX,
            max_ts: 0,
            source_bitmap: 0,
        })
    }

    fn push(&mut self, frame: &[u8], ev: &Event) -> io::Result<()> {
        self.enc.write_all(frame)?;
        self.event_count += 1;
        self.uncompressed_bytes += frame.len() as u64;
        self.min_ts = self.min_ts.min(ev.ts_mono_ns);
        self.max_ts = self.max_ts.max(ev.ts_mono_ns);
        self.source_bitmap |= 1 << ev.source.as_u8();
        Ok(())
    }
}

/// A sealed unit waiting to be appended.
struct Unit {
    bytes: Vec<u8>,
    events: u32,
    /// Chunked only; `offset` is filled in once the unit is on disk.
    entry: Option<ChunkIndexEntry>,
}

/// The events file plus the units that could not be written yet.
struct Appender {
    file: File,
    /// End of the last unit fully on disk.
    cursor: u64,
    pending: VecDeque<Unit>,
    pending_bytes: usize,
    /// `MAX_PENDING_BYTES`; a field so tests can make it small.
    max_pending_bytes: usize,
    dropped_events: u64,
    last_error: Option<String>,
    index: Vec<ChunkIndexEntry>,
}

impl Appender {
    fn new(file: File, cursor: u64) -> Self {
        Self {
            file,
            cursor,
            pending: VecDeque::new(),
            pending_bytes: 0,
            max_pending_bytes: MAX_PENDING_BYTES,
            dropped_events: 0,
            last_error: None,
            index: Vec::new(),
        }
    }

    /// Queue a unit; drops it (and counts its events) when the backlog is
    /// already at `max_pending_bytes`. Returns whether it was kept.
    fn enqueue(&mut self, unit: Unit) -> bool {
        if !self.pending.is_empty()
            && self.pending_bytes + unit.bytes.len() > self.max_pending_bytes
        {
            self.dropped_events += u64::from(unit.events);
            return false;
        }
        self.pending_bytes += unit.bytes.len();
        self.pending.push_back(unit);
        true
    }

    /// Write every pending unit, in order. On failure the file is cut back
    /// to `cursor` so a partly written unit never stays on disk.
    fn drain(&mut self) -> io::Result<()> {
        while let Some(unit) = self.pending.front() {
            let res = self
                .file
                .seek(SeekFrom::Start(self.cursor))
                .and_then(|_| self.file.write_all(&unit.bytes));
            if let Err(e) = res {
                if let Err(te) = self.file.set_len(self.cursor) {
                    tracing::warn!(error = %te, "could not cut a partly written unit");
                }
                self.last_error = Some(e.to_string());
                return Err(e);
            }
            let Some(unit) = self.pending.pop_front() else {
                break;
            };
            self.pending_bytes -= unit.bytes.len();
            if let Some(mut entry) = unit.entry {
                entry.offset = self.cursor;
                self.index.push(entry);
            }
            self.cursor += unit.bytes.len() as u64;
        }
        self.last_error = None;
        Ok(())
    }
}

enum Kind {
    Legacy,
    Chunked(ChunkConfig),
}

pub struct SessionWriter {
    dir: PathBuf,
    metadata: SessionMetadata,
    kind: Kind,
    batch: Batch,
    out: Appender,
}

impl SessionWriter {
    pub fn create(metadata: SessionMetadata) -> std::io::Result<Self> {
        Self::create_with_format(metadata, false)
    }

    /// `chunked_requested` is the per-session opt-in forwarded by the record
    /// client (#188 — SMELTR_SESSION_INDEX set on the client used to be
    /// silently ignored because only the daemon's env was consulted). The
    /// daemon-side env remains the global default: chunked = request OR env.
    pub fn create_with_format(
        metadata: SessionMetadata,
        chunked_requested: bool,
    ) -> std::io::Result<Self> {
        let chunked =
            chunked_requested || std::env::var("SMELTR_SESSION_INDEX").as_deref() == Ok("1");
        Self::create_with_chunk_config(metadata, chunked.then(ChunkConfig::default))
    }

    /// `None` writes the legacy format: a sequence of zstd frames.
    pub fn create_with_chunk_config(
        metadata: SessionMetadata,
        cfg: Option<ChunkConfig>,
    ) -> std::io::Result<Self> {
        let dir = session_dir(&metadata);
        create_dir_all(&dir)?;
        let (kind, out) = match cfg {
            Some(cfg) => {
                let mut file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(events_path_zst(&dir))?;
                file.write_all(&chunked::HEAD_MAGIC)?;
                let cursor = chunked::HEAD_MAGIC.len() as u64;
                (Kind::Chunked(cfg), Appender::new(file, cursor))
            }
            None => {
                let file = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(false)
                    .open(events_path_zst(&dir))?;
                let cursor = file.metadata()?.len();
                (Kind::Legacy, Appender::new(file, cursor))
            }
        };
        let w = Self {
            dir,
            metadata,
            kind,
            batch: Batch::new()?,
            out,
        };
        w.persist_metadata()?;
        Ok(w)
    }

    /// Uncompressed bytes after which the current batch is sealed.
    ///
    /// Chunked batches never exceed one frame's maximum, so every chunk the
    /// writer produces fits `chunked::MAX_CHUNK_BYTES`, the bound the recovery
    /// scan accepts.
    fn batch_limit(&self) -> u64 {
        match self.kind {
            Kind::Legacy => LEGACY_FRAME_BYTES,
            Kind::Chunked(cfg) => cfg.max_bytes.min(u64::from(MAX_FRAME_BYTES)),
        }
    }

    /// Accepts an event. `Ok` means the writer holds it — in memory until the
    /// next seal or flush writes it. An error means it was not kept: it could
    /// not be encoded, or the disk has refused writes for so long that the
    /// in-memory backlog is full and the unit holding it was dropped.
    pub fn write_event(&mut self, ev: &Event) -> Result<(), CodecError> {
        let frame = encode_frame(ev)?;
        let limit = self.batch_limit();
        if self.batch.event_count > 0 && self.batch.uncompressed_bytes + frame.len() as u64 > limit
        {
            self.seal_and_try_drain()?;
        }
        self.batch.push(&frame, ev)?;
        let full = match self.kind {
            Kind::Legacy => self.batch.uncompressed_bytes >= limit,
            Kind::Chunked(cfg) => {
                self.batch.event_count >= cfg.max_events || self.batch.uncompressed_bytes >= limit
            }
        };
        if full {
            self.seal_and_try_drain()?;
        }
        Ok(())
    }

    /// Seal, then attempt the write. A failed write is not an error here: the
    /// unit stays pending and `flush` reports it.
    fn seal_and_try_drain(&mut self) -> io::Result<()> {
        self.seal()?;
        let _ = self.out.drain();
        Ok(())
    }

    /// Turn the current batch into a unit and queue it. Errors when the unit
    /// had to be dropped.
    fn seal(&mut self) -> io::Result<()> {
        if self.batch.event_count == 0 {
            return Ok(());
        }
        let batch = std::mem::replace(&mut self.batch, Batch::new()?);
        let events = batch.event_count;
        let (min_ts, max_ts, source_bitmap) = (batch.min_ts, batch.max_ts, batch.source_bitmap);
        let compressed = match batch.enc.finish() {
            Ok(b) => b,
            Err(e) => {
                self.out.dropped_events += u64::from(events);
                return Err(e);
            }
        };
        let unit = match self.kind {
            Kind::Legacy => Unit {
                bytes: compressed,
                events,
                entry: None,
            },
            Kind::Chunked(_) => {
                let comp_len = u32::try_from(compressed.len())
                    .ok()
                    .filter(|&n| u64::from(n) <= chunked::MAX_CHUNK_BYTES);
                let Some(comp_len) = comp_len else {
                    self.out.dropped_events += u64::from(events);
                    return Err(io::Error::other(format!(
                        "chunk of {} bytes exceeds MAX_CHUNK_BYTES",
                        compressed.len()
                    )));
                };
                let mut bytes = Vec::with_capacity(4 + compressed.len());
                bytes.extend_from_slice(&comp_len.to_le_bytes());
                bytes.extend_from_slice(&compressed);
                Unit {
                    bytes,
                    events,
                    entry: Some(ChunkIndexEntry {
                        offset: 0,
                        comp_len,
                        min_ts,
                        max_ts,
                        source_bitmap,
                        event_count: events,
                    }),
                }
            }
        };
        // Make room first: the disk may have recovered since the last try.
        if !self.out.pending.is_empty() {
            let _ = self.out.drain();
        }
        if self.out.enqueue(unit) {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "disk refusing writes and {} bytes already pending: {events} events dropped",
                self.out.pending_bytes
            )))
        }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// The last write error, while units are still waiting for the disk.
    /// `None` once everything pending has been written.
    pub fn storage_error(&self) -> Option<&str> {
        self.out.last_error.as_deref()
    }

    /// Accepted events dropped so far because the backlog was full.
    pub fn dropped_events(&self) -> u64 {
        self.out.dropped_events
    }

    /// Makes every accepted event durable: seals the current batch — even a
    /// single event, so a daemon SIGKILL right after loses nothing that was
    /// flushed (#268) — and writes everything pending.
    pub fn flush(&mut self) -> std::io::Result<()> {
        let min = match self.kind {
            Kind::Legacy => 0,
            Kind::Chunked(cfg) => cfg.flush_min_bytes,
        };
        let sealed = if self.batch.event_count > 0 && self.batch.uncompressed_bytes >= min {
            self.seal()
        } else {
            Ok(())
        };
        self.out.drain()?;
        sealed?;
        self.out.file.flush()
    }

    /// Finalize, recording the signal that killed the child when it died by
    /// one (#203).
    ///
    /// `finalize` remains the plain form; this sibling exists so the dozens of
    /// existing call sites keep working unchanged, and only the record path —
    /// the one place that actually observes a signal — passes it.
    pub fn finalize_with_signal(
        mut self,
        exit_code: Option<i32>,
        term_signal: Option<i32>,
        ended_rfc3339: String,
    ) -> std::io::Result<()> {
        self.metadata.term_signal = term_signal;
        self.finalize(exit_code, ended_rfc3339)
    }

    /// Writes everything accepted, the chunked footer, and the metadata.
    /// Units the disk still refuses are counted as dropped; the metadata is
    /// written regardless, and the first error is returned.
    pub fn finalize(
        mut self,
        exit_code: Option<i32>,
        ended_rfc3339: String,
    ) -> std::io::Result<()> {
        let mut first_err = self.seal().err();
        if let Err(e) = self.out.drain() {
            let lost: u64 = self.out.pending.iter().map(|u| u64::from(u.events)).sum();
            self.out.dropped_events += lost;
            first_err.get_or_insert(e);
        } else if let Kind::Chunked(_) = self.kind {
            if self.out.index.len() > chunked::MAX_CHUNKS {
                tracing::warn!(
                    "chunked session has {} chunks, exceeds MAX_CHUNKS={}",
                    self.out.index.len(),
                    chunked::MAX_CHUNKS
                );
            }
            let out = &mut self.out;
            let res = out
                .file
                .seek(SeekFrom::Start(out.cursor))
                .and_then(|_| chunked::write_footer_at(&mut out.file, out.cursor, &out.index))
                .and_then(|_| out.file.flush());
            if let Err(e) = res {
                // No footer is fine: the chunks stay scan-recoverable. A torn
                // one is not, so cut it.
                let _ = out.file.set_len(out.cursor);
                first_err.get_or_insert(e);
            }
        }
        self.metadata.exit_code = exit_code;
        self.metadata.ended_rfc3339 = Some(ended_rfc3339);
        if self.out.dropped_events > 0 {
            self.metadata.dropped_events = Some(self.out.dropped_events);
        }
        let meta = self.persist_metadata();
        match first_err {
            Some(e) => Err(e),
            None => meta,
        }
    }

    fn persist_metadata(&self) -> std::io::Result<()> {
        crate::session::write_metadata(&self.dir, &self.metadata)
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

    /// Swap the events file for a read-only handle: every write then fails
    /// for real (EBADF), with no mock in between.
    fn break_disk(w: &mut SessionWriter) {
        let path = crate::session::events_path_zst(&w.dir);
        w.out.file = std::fs::File::open(path).unwrap();
    }

    fn repair_disk(w: &mut SessionWriter) {
        let path = crate::session::events_path_zst(&w.dir);
        w.out.file = OpenOptions::new().write(true).open(path).unwrap();
    }

    /// #268: a failed seal is transient. Chunks wait in memory up to the
    /// backlog cap; beyond it they are dropped, counted, and the count lands
    /// in the metadata. Once writes succeed again, everything kept is written.
    #[test]
    #[serial]
    fn chunked_backlog_is_bounded_and_drops_are_counted() {
        let _home = temp_home();
        let meta = SessionMetadata::now_starting(SessionId::new());
        let cfg = crate::chunked::ChunkConfig {
            max_events: 10,
            max_bytes: 1 << 30,
            flush_min_bytes: 0,
        };
        let mut w = SessionWriter::create_with_chunk_config(meta, Some(cfg)).unwrap();
        let dir = w.dir().to_path_buf();
        for i in 0..10u64 {
            w.write_event(&ev(i, Source::Mark)).unwrap();
        }
        assert!(w.storage_error().is_none());

        break_disk(&mut w);
        // Room for one more chunk only.
        w.out.max_pending_bytes = 1;
        let mut refused = 0;
        for i in 10..100u64 {
            if w.write_event(&ev(i, Source::Mark)).is_err() {
                refused += 1;
            }
        }
        assert!(w.storage_error().is_some(), "the failure must be visible");
        assert!(w.flush().is_err());
        assert_eq!(w.dropped_events(), 80, "8 of the 9 chunks dropped");
        assert_eq!(refused, 8, "each dropped chunk is reported once");

        repair_disk(&mut w);
        for i in 100..105u64 {
            w.write_event(&ev(i, Source::Mark)).unwrap();
        }
        w.flush().unwrap();
        assert!(w.storage_error().is_none(), "recovered");
        w.finalize(Some(0), "end".into()).unwrap();

        let seqs: Vec<u64> = crate::reader::read_events(&dir)
            .unwrap()
            .into_iter()
            .map(|e| e.seq)
            .collect();
        let want: Vec<u64> = (0..20).chain(100..105).collect();
        assert_eq!(seqs, want);
        let meta = crate::reader::read_metadata(&dir).unwrap();
        assert_eq!(meta.dropped_events, Some(80));
    }

    /// Same for the legacy format: the failed frame is kept and written
    /// once the disk accepts it, and the stream stays decodable.
    #[test]
    #[serial]
    fn legacy_failed_write_is_retried_not_lost() {
        let _home = temp_home();
        let meta = SessionMetadata::now_starting(SessionId::new());
        let mut w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        w.write_event(&ev(0, Source::Mark)).unwrap();
        w.flush().unwrap();
        break_disk(&mut w);
        for i in 1..50u64 {
            w.write_event(&ev(i, Source::Mark)).unwrap();
        }
        assert!(w.flush().is_err());
        repair_disk(&mut w);
        w.write_event(&ev(50, Source::Mark)).unwrap();
        w.finalize(Some(0), "end".into()).unwrap();
        let seqs: Vec<u64> = crate::reader::read_events(&dir)
            .unwrap()
            .into_iter()
            .map(|e| e.seq)
            .collect();
        assert_eq!(seqs, (0..51).collect::<Vec<_>>());
    }

    /// #268: the daemon flushes every 500 ms; with the default config a
    /// flush must make even a handful of events durable. A SIGKILL (the
    /// writer dropped without finalize) used to lose everything under
    /// FLUSH_MIN_BYTES — a whole short chunked run.
    #[test]
    #[serial]
    fn chunked_default_flush_makes_few_events_durable() {
        let _home = temp_home();
        let meta = SessionMetadata::now_starting(SessionId::new());
        let mut w = SessionWriter::create_with_format(meta, true).unwrap();
        let dir = w.dir().to_path_buf();
        for i in 0..3u64 {
            w.write_event(&ev(i, Source::Mark)).unwrap();
        }
        w.flush().unwrap();
        drop(w); // SIGKILL: no finalize, no footer
        assert_eq!(crate::reader::read_events(&dir).unwrap().len(), 3);
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

#[cfg(test)]
mod format_request_tests {
    use super::*;
    use crate::session::{SessionId, SessionMetadata};

    /// #188: the per-session request must win even when the daemon env is
    /// unset — that's the record-client path.
    #[test]
    #[serial_test::serial]
    fn requested_chunked_without_env_writes_chunked() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_INDEX");
        let meta = SessionMetadata::now_starting(SessionId::new());
        let w = SessionWriter::create_with_format(meta, true).unwrap();
        w.finalize(Some(0), "x".into()).unwrap();
        let path = crate::reader::list_sessions().unwrap().pop().unwrap();
        let bytes = std::fs::read(path.join("events.cbor.zst")).unwrap();
        assert!(
            bytes.starts_with(&crate::chunked::HEAD_MAGIC),
            "chunked head magic expected"
        );
    }

    #[test]
    #[serial_test::serial]
    fn no_request_no_env_stays_legacy() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        std::env::remove_var("SMELTR_SESSION_INDEX");
        let meta = SessionMetadata::now_starting(SessionId::new());
        let w = SessionWriter::create_with_format(meta, false).unwrap();
        w.finalize(Some(0), "x".into()).unwrap();
        let path = crate::reader::list_sessions().unwrap().pop().unwrap();
        let bytes = std::fs::read(path.join("events.cbor.zst")).unwrap();
        assert!(
            !bytes.starts_with(&crate::chunked::HEAD_MAGIC),
            "legacy zstd stream expected"
        );
    }
}

//! Session reader. Used by `smeltr sessions show`, replay, the analyzer and MCP.

use crate::chunked::ChunkIndexEntry;
use crate::codec::CodecError;
use crate::event::Event;
use crate::filter::EventFilter;
use crate::session::{metadata_path, sessions_root, SessionId, SessionMetadata};
use std::fs::File;
use std::io::Read;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

/// Lists every session directory under `sessions_root()`.
pub fn list_sessions() -> std::io::Result<Vec<PathBuf>> {
    let root = sessions_root();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

/// Finds a session's directory by id: by the short-id suffix every writer
/// puts in the name, else by the id in each directory's metadata — a
/// renamed or hand-built directory has no such suffix (#268).
pub fn find_session_dir(id: SessionId) -> std::io::Result<Option<PathBuf>> {
    let short = id.short();
    let dirs = list_sessions()?;
    for dir in &dirs {
        if dir
            .file_name()
            .map(|n| n.to_string_lossy().ends_with(&short))
            .unwrap_or(false)
            && read_metadata(dir).map_or(true, |m| m.session_id == id)
        {
            return Ok(Some(dir.clone()));
        }
    }
    Ok(dirs
        .into_iter()
        .find(|dir| read_metadata(dir).is_ok_and(|m| m.session_id == id)))
}

pub fn read_metadata(dir: &Path) -> std::io::Result<SessionMetadata> {
    let text = std::fs::read_to_string(metadata_path(dir))?;
    // Keep TOML's own message (field, line): "could not parse" alone left
    // nothing to act on (#245).
    toml::from_str::<SessionMetadata>(&text).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("could not parse metadata.toml: {e}"),
        )
    })
}

/// What a reader could not read. Present means the events returned are a
/// subset of what the session recorded (#268): analyses built on them must
/// say so rather than present a truncated session as complete.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Damage {
    pub problems: Vec<String>,
}

impl std::fmt::Display for Damage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.problems.join("; "))
    }
}

/// Every event still readable from the session, and what could not be read.
///
/// Damaged or truncated data never fails the read: each unit (legacy zstd
/// frame, chunk) that decodes is returned, damaged ones are skipped and
/// described. Units written since #268 carry a zstd checksum, so a flipped
/// byte costs its unit instead of decoding to wrong values. Errors are left
/// for a missing file and a format from a newer smeltr.
pub fn read_events_checked(dir: &Path) -> std::io::Result<(Vec<Event>, Option<Damage>)> {
    let mut events = Vec::new();
    let damage = for_each_event(dir, None, |e| {
        events.push(e);
        ControlFlow::Continue(())
    })?;
    Ok((events, damage))
}

/// [`read_events_checked`] for callers that only want the events; damage is
/// logged.
pub fn read_events(dir: &Path) -> std::io::Result<Vec<Event>> {
    let (events, damage) = read_events_checked(dir)?;
    if let Some(d) = damage {
        tracing::warn!(dir = ?dir, "session data is incomplete: {d}");
    }
    Ok(events)
}

/// Hands the session's readable events to `f`, in file order, without
/// holding them all in memory; `f` returns `ControlFlow::Break` to stop
/// early. With a `filter`, only matching events reach `f`, and a chunked
/// session with a footer skips the chunks that cannot match.
///
/// Returns what could not be read, exactly as [`read_events_checked`] does
/// for a complete pass without a filter. After an early stop, or with a
/// filter, the damage covers only what was read, and the metadata's event
/// count is not checked.
///
/// Event frames are decoded on several threads (#271): CBOR decoding, not
/// zstd, was ~90 % of reading a session (7.3 s of 8.1 s for 3.3 M events).
/// A pre-#268 legacy file — one unchecked zstd frame, the whole session —
/// is decompressed 16 MB at a time rather than whole.
pub fn for_each_event<F>(
    dir: &Path,
    filter: Option<&EventFilter>,
    mut f: F,
) -> std::io::Result<Option<Damage>>
where
    F: FnMut(Event) -> ControlFlow<()>,
{
    let path = crate::session::events_path_for_read(dir);
    let buf = std::fs::read(&path)?;
    let mut sink = Sink {
        filter,
        f: &mut f,
        seen: 0,
        stopped: false,
    };
    let mut problems = Vec::new();
    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        match crate::chunked::detect_bytes(&buf) {
            crate::chunked::Format::Chunked => read_chunked(&buf, filter, &mut sink, &mut problems),
            crate::chunked::Format::Unsupported(v) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("session written by a newer smeltr (format v{v}); upgrade to read it"),
                ));
            }
            crate::chunked::Format::Legacy => read_legacy_zst(&buf, &mut sink, &mut problems),
        }
    } else {
        // Uncompressed .cbor from the first releases.
        let mut parser = FrameParser::new(0);
        parser.feed(&buf, &mut sink, &mut problems);
        parser.finish(&mut sink, &mut problems);
    }
    if filter.is_none() && !sink.stopped {
        if let Ok(meta) = read_metadata(dir) {
            if let Some(written) = meta.event_count {
                let got = sink.seen;
                if got < written {
                    problems.push(format!(
                        "{} of the {written} events written are missing",
                        written - got
                    ));
                }
            }
            if let Some(dropped) = meta.dropped_events.filter(|&d| d > 0) {
                problems.push(format!(
                    "{dropped} events were dropped while the disk refused writes"
                ));
            }
        }
    }
    Ok((!problems.is_empty()).then_some(Damage { problems }))
}

/// Where decoded events go: the filter, the caller's callback, and whether
/// it asked to stop.
struct Sink<'a> {
    filter: Option<&'a EventFilter>,
    f: &'a mut dyn FnMut(Event) -> ControlFlow<()>,
    /// Events decoded (before the filter).
    seen: u64,
    stopped: bool,
}

impl Sink<'_> {
    fn push(&mut self, e: Event) {
        if self.stopped {
            return;
        }
        self.seen += 1;
        if self.filter.is_some_and(|flt| !flt.matches(&e)) {
            return;
        }
        if (self.f)(e).is_break() {
            self.stopped = true;
        }
    }
}

/// Decompressed bytes handed to the frame decoder at a time.
const BLOCK_BYTES: usize = 16 << 20;
/// Fewest complete frames in a batch worth spreading over threads.
const PARALLEL_MIN_FRAMES: usize = 2048;

/// Decode threads: measured on an M2 Pro (6P+4E), 8 threads read 3.3 M
/// events in 3.0 s against 9.0 s on one; 10 was no faster.
fn decode_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(8)
}

/// Decodes the length-prefixed event frames of one unit's decompressed
/// data, fed in pieces. Semantics are those of reading the whole data with
/// [`crate::codec::read_frame`]: the events before the first undecodable
/// byte are kept, and the problem names the byte where the read stopped.
struct FrameParser {
    /// Byte offset of the unit in the file, for the problem message.
    at: usize,
    /// Bytes not yet decoded (a frame cut by the end of a piece).
    carry: Vec<u8>,
    /// Offset of `carry[0]` in the unit's data.
    base: usize,
    failed: bool,
}

impl FrameParser {
    fn new(at: usize) -> Self {
        Self {
            at,
            carry: Vec::new(),
            base: 0,
            failed: false,
        }
    }

    fn feed(&mut self, data: &[u8], sink: &mut Sink, problems: &mut Vec<String>) {
        if self.failed || sink.stopped {
            return;
        }
        self.carry.extend_from_slice(data);
        let (frames, consumed, bad) = split_frames(&self.carry);
        let decoded = decode_frames_in(&self.carry, &frames);
        for (i, r) in decoded.into_iter().enumerate() {
            match r {
                Ok(e) => sink.push(e),
                Err(e) => {
                    let (start, len) = frames[i];
                    self.fail(start + 4 + len, &e, problems);
                    return;
                }
            }
            if sink.stopped {
                return;
            }
        }
        if let Some((pos, e)) = bad {
            self.fail(pos, &e, problems);
            return;
        }
        self.carry.drain(..consumed);
        self.base += consumed;
    }

    /// End of the unit's data: a frame cut short is a truncation.
    fn finish(self, _sink: &mut Sink, problems: &mut Vec<String>) {
        if !self.failed && !self.carry.is_empty() {
            let end = self.base + self.carry.len();
            problems.push(format!(
                "undecodable event data at byte {end} of the frame at {}: {}",
                self.at,
                CodecError::Truncated
            ));
        }
    }

    fn fail(&mut self, pos_in_carry: usize, e: &CodecError, problems: &mut Vec<String>) {
        problems.push(format!(
            "undecodable event data at byte {} of the frame at {}: {e}",
            self.base + pos_in_carry,
            self.at
        ));
        self.failed = true;
        self.carry = Vec::new();
    }
}

/// The complete frames at the start of `buf` as (start, payload length),
/// the bytes they span, and a frame header that can never be valid (with
/// the position where `read_frame` would have stopped).
/// (start, payload length) of each complete frame, the bytes they span, and
/// an impossible frame header with where `read_frame` would have stopped.
type Split = (Vec<(usize, usize)>, usize, Option<(usize, CodecError)>);

fn split_frames(buf: &[u8]) -> Split {
    let mut frames = Vec::new();
    let mut p = 0usize;
    while let Some(header) = buf.get(p..p + 4) {
        let mut len = [0u8; 4];
        len.copy_from_slice(header);
        let len = u32::from_le_bytes(len);
        if len > crate::codec::MAX_FRAME_BYTES {
            let e = CodecError::FrameTooLarge(len, crate::codec::MAX_FRAME_BYTES);
            return (frames, p, Some((p + 4, e)));
        }
        let end = p + 4 + len as usize;
        if end > buf.len() {
            break;
        }
        frames.push((p, len as usize));
        p = end;
    }
    (frames, p, None)
}

fn decode_frames_in(buf: &[u8], frames: &[(usize, usize)]) -> Vec<Result<Event, CodecError>> {
    let decode = |&(start, len): &(usize, usize)| -> Result<Event, CodecError> {
        Ok(ciborium::from_reader(&buf[start + 4..start + 4 + len])?)
    };
    let threads = decode_threads();
    if frames.len() < PARALLEL_MIN_FRAMES || threads < 2 {
        return frames.iter().map(decode).collect();
    }
    let per = frames.len().div_ceil(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = frames
            .chunks(per)
            .map(|part| s.spawn(move || part.iter().map(decode).collect::<Vec<_>>()))
            .collect();
        let mut out = Vec::with_capacity(frames.len());
        for h in handles {
            match h.join() {
                Ok(part) => out.extend(part),
                // A panic in the decoder: report it as undecodable data.
                Err(_) => out.push(Err(CodecError::Io(std::io::Error::other(
                    "frame decoder thread panicked",
                )))),
            }
        }
        out
    })
}

/// Decompress one zstd frame; on error, the output produced before it.
fn decompress_frame(frame: &[u8]) -> (Vec<u8>, Option<std::io::Error>) {
    let mut data = Vec::new();
    let res = zstd::stream::read::Decoder::with_buffer(frame)
        .map(|d| d.single_frame())
        .and_then(|mut d| d.read_to_end(&mut data));
    (data, res.err())
}

/// Stream one unchecked zstd frame through `parser`, `BLOCK_BYTES` of
/// output at a time; the error that ended it, if any.
///
/// The output of a decoder call that fails is lost with the call, so after
/// a failure the frame is decoded again up to where the first pass stopped
/// and then one byte per call: every byte the decoder can produce before
/// the damage goes through — never less than [`decompress_frame`] gave,
/// whose growing buffer lost whatever its last call had decoded.
fn stream_frame(
    frame: &[u8],
    parser: &mut FrameParser,
    sink: &mut Sink,
    problems: &mut Vec<String>,
) -> Option<std::io::Error> {
    let mut dec = match zstd::stream::read::Decoder::with_buffer(frame) {
        Ok(d) => d.single_frame(),
        Err(e) => return Some(e),
    };
    let mut step = vec![0u8; STEP_BYTES];
    let mut block = Vec::new();
    let mut fed = 0usize;
    loop {
        block.clear();
        let mut end = None;
        while block.len() < BLOCK_BYTES {
            match dec.read(&mut step) {
                Ok(0) => {
                    end = Some(None);
                    break;
                }
                Ok(n) => block.extend_from_slice(&step[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    end = Some(Some(e));
                    break;
                }
            }
        }
        // Once the sink stopped, nothing more is needed; after a parse
        // failure the frame is still drained to learn how it ends.
        parser.feed(&block, sink, problems);
        fed += block.len();
        if sink.stopped {
            return None;
        }
        match end {
            None => {}
            Some(None) => return None,
            Some(Some(e)) => {
                parser.feed(&output_before_damage(frame, fed), sink, problems);
                return Some(e);
            }
        }
    }
}

/// What the decoder produces after the first `fed` bytes of `frame`'s
/// output and before its damage, read one byte per call so that no decoded
/// byte is lost with the failing call.
fn output_before_damage(frame: &[u8], fed: usize) -> Vec<u8> {
    let Ok(dec) = zstd::stream::read::Decoder::with_buffer(frame) else {
        return Vec::new();
    };
    let mut dec = dec.single_frame();
    let mut buf = vec![0u8; STEP_BYTES];
    let mut skipped = 0;
    // Never ask past `fed`: those calls succeeded the first time.
    while skipped < fed {
        let want = (fed - skipped).min(STEP_BYTES);
        match dec.read(&mut buf[..want]) {
            Ok(0) | Err(_) => return Vec::new(),
            Ok(n) => skipped += n,
        }
    }
    let mut tail = Vec::new();
    let mut byte = [0u8; 1];
    while let Ok(1) = dec.read(&mut byte) {
        tail.push(byte[0]);
    }
    tail
}

/// Output per decoder call in the first pass.
const STEP_BYTES: usize = 64 << 10;

fn find_magic(buf: &[u8], from: usize) -> Option<usize> {
    buf.get(from..)?
        .windows(4)
        .position(|w| w == crate::chunked::ZSTD_MAGIC)
        .map(|i| i + from)
}

/// A legacy events file: zstd frames back to back. Since #268 each frame is
/// checksummed and ends on an event boundary; before, the whole session was
/// one unchecked frame.
///
/// A checksummed frame is all or nothing. An unchecked frame is trusted only
/// at offset 0, where the old writer put its single frame, and keeps the
/// events decoded before any damage — it used to lose the whole session. A
/// damaged region is skipped up to the next checksummed frame.
fn read_legacy_zst(buf: &[u8], sink: &mut Sink, problems: &mut Vec<String>) {
    use crate::chunked::{frame_has_checksum, ZSTD_MAGIC};
    let mut pos = 0;
    while pos < buf.len() && !sink.stopped {
        let rest = &buf[pos..];
        let checksummed = frame_has_checksum(rest);
        let trusted = rest.starts_with(&ZSTD_MAGIC) && (checksummed || pos == 0);
        if !trusted {
            let next = (pos + 1..buf.len()).find(|&p| frame_has_checksum(&buf[p..]));
            match next {
                Some(p) => {
                    problems.push(format!("bytes {pos}..{p} of the event file are unreadable"));
                    pos = p;
                    continue;
                }
                None => {
                    problems.push(format!(
                        "the last {} bytes of the event file are unreadable",
                        buf.len() - pos
                    ));
                    break;
                }
            }
        }
        let size = zstd::zstd_safe::find_frame_compressed_size(rest)
            .ok()
            .filter(|&n| n > 0 && n <= rest.len());
        let frame = &rest[..size.unwrap_or(rest.len())];
        let mut parser = FrameParser::new(pos);
        let err = if checksummed {
            // All or nothing: nothing reaches the sink before the checksum
            // has been verified at the end of the frame.
            let (data, err) = decompress_frame(frame);
            if size.is_some() && err.is_none() {
                parser.feed(&data, sink, problems);
            }
            err
        } else {
            stream_frame(frame, &mut parser, sink, problems)
        };
        if sink.stopped {
            break;
        }
        parser.finish(sink, problems);
        if let (Some(n), None) = (size, &err) {
            pos += n;
            continue;
        }
        let what = match (size, err) {
            (Some(_), Some(e)) => format!("is damaged ({e})"),
            _ => "is incomplete (truncated or damaged)".to_string(),
        };
        problems.push(format!(
            "the zstd frame at byte {pos} {what}; {}",
            if checksummed {
                "its events were skipped"
            } else {
                "the events after the damage are lost"
            }
        ));
        // Resume at the next frame; searching from pos + 1 rather than
        // jumping by a size read from damaged bytes.
        match find_magic(buf, pos + 1) {
            Some(p) => pos = p,
            None => break,
        }
    }
}

/// A chunked events file: through its footer index when it has a valid one,
/// else by scanning.
fn read_chunked(
    buf: &[u8],
    filter: Option<&EventFilter>,
    sink: &mut Sink,
    problems: &mut Vec<String>,
) {
    use crate::chunked::SessionFormatError;
    let scanned = match crate::chunked::read_footer_with_offset(&mut std::io::Cursor::new(buf)) {
        Ok(Some((entries, footer_offset))) => {
            return decode_indexed(buf, &entries, footer_offset, filter, sink, problems)
        }
        Ok(None) => crate::chunked::scan_chunks_checked(buf, buf.len(), problems),
        Err(SessionFormatError::FooterCorrupt(m)) => {
            tracing::warn!("{m}; falling back to chunk scan");
            let end = crate::chunked::claimed_footer_offset(buf).unwrap_or(buf.len());
            crate::chunked::scan_chunks_checked(buf, end, problems)
        }
        Err(SessionFormatError::Io(e)) => {
            problems.push(format!("footer unreadable: {e}"));
            crate::chunked::scan_chunks_checked(buf, buf.len(), problems)
        }
    };
    for e in scanned {
        sink.push(e);
        if sink.stopped {
            break;
        }
    }
}

/// Decode the chunks a footer indexes. Lengths come from the CRC-protected
/// index — the `comp_len` prefix in the file is not protected. A chunk that
/// does not decode is skipped and reported; the others are still read (a
/// single bad chunk used to fail the whole read).
fn decode_indexed(
    buf: &[u8],
    entries: &[ChunkIndexEntry],
    footer_offset: u64,
    filter: Option<&EventFilter>,
    sink: &mut Sink,
    problems: &mut Vec<String>,
) {
    for (i, entry) in entries.iter().enumerate() {
        if sink.stopped {
            return;
        }
        if let Some(f) = filter {
            if !f.chunk_overlaps(entry) {
                continue;
            }
        }
        let end = entry.offset + 4 + u64::from(entry.comp_len);
        if entry.offset < crate::chunked::HEAD_MAGIC.len() as u64 || end > footer_offset {
            problems.push(format!(
                "chunk {i} ({} events) lies outside the chunk area",
                entry.event_count
            ));
            continue;
        }
        let start = usize::try_from(entry.offset + 4).unwrap_or(usize::MAX);
        let decoded = buf
            .get(start..start.saturating_add(entry.comp_len as usize))
            .ok_or_else(|| std::io::Error::other("chunk past the end of the file"))
            .and_then(crate::chunked::decode_chunk);
        match decoded {
            Ok(events) => {
                if events.len() != entry.event_count as usize {
                    problems.push(format!(
                        "chunk {i}: {} events decoded, {} indexed",
                        events.len(),
                        entry.event_count
                    ));
                }
                for e in events {
                    sink.push(e);
                    if sink.stopped {
                        return;
                    }
                }
            }
            Err(e) => problems.push(format!(
                "chunk {i} ({} events) is unreadable: {e}",
                entry.event_count
            )),
        }
    }
}

/// Like `read_events` but applies an `EventFilter`, using the chunk index to
/// skip irrelevant chunks when the session is sealed and chunked.
pub fn read_events_filtered(dir: &Path, filter: &EventFilter) -> std::io::Result<Vec<Event>> {
    let mut out = Vec::new();
    let damage = for_each_event(dir, Some(filter), |e| {
        out.push(e);
        ControlFlow::Continue(())
    })?;
    if let Some(d) = damage {
        tracing::warn!(dir = ?dir, "session data is incomplete: {d}");
    }
    Ok(out)
}

/// The event count a sealed chunked session's footer index records: no
/// decoding at all. `None` for any other session.
pub fn indexed_event_count(dir: &Path) -> std::io::Result<Option<usize>> {
    let path = crate::session::events_path_for_read(dir);
    let mut f = File::open(&path)?;
    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        if let crate::chunked::Format::Chunked = crate::chunked::detect(&mut f)? {
            if let Ok(Some(entries)) = crate::chunked::read_footer(&mut f) {
                return Ok(Some(entries.iter().map(|e| e.event_count as usize).sum()));
            }
        }
    }
    Ok(None)
}

/// Return the total number of events in the session.
///
/// For sealed chunked sessions this is O(chunks) via the footer index;
/// otherwise every event is decoded (but none is kept).
pub fn session_event_count(dir: &Path) -> std::io::Result<usize> {
    if let Some(n) = indexed_event_count(dir)? {
        return Ok(n);
    }
    let mut n = 0usize;
    for_each_event(dir, None, |_| {
        n += 1;
        ControlFlow::Continue(())
    })?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunked::ChunkConfig;
    use crate::event::{Payload, Source};
    use crate::filter::EventFilter;
    use crate::session::{SessionId, SessionKind, SessionMetadata};
    use crate::writer::SessionWriter;
    use serial_test::serial;
    use std::path::PathBuf;
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

    /// Write a session with the given events.
    ///
    /// When `chunked` is `true` the session uses a small `ChunkConfig`
    /// (max_events=64) so 2500 events span many chunks.  When `false` the
    /// legacy streaming zstd writer is used.
    fn write_session(evs: &[Event], chunked: bool) -> PathBuf {
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let cfg = if chunked {
            Some(ChunkConfig {
                max_events: 64,
                max_bytes: crate::chunked::CHUNK_BYTES,
                flush_min_bytes: crate::chunked::FLUSH_MIN_BYTES,
            })
        } else {
            None
        };
        let mut w = SessionWriter::create_with_chunk_config(meta, cfg).unwrap();
        for e in evs {
            w.write_event(e).unwrap();
        }
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "2026-06-23T00:00:00Z".into()).unwrap();
        dir
    }

    #[test]
    #[serial]
    fn reads_chunked_identically_to_legacy() {
        let _home = temp_home();
        let evs: Vec<Event> = (0..2500u64)
            .map(|i| {
                ev(
                    i,
                    if i % 2 == 0 {
                        Source::Mark
                    } else {
                        Source::MetalHook
                    },
                )
            })
            .collect();
        let legacy = write_session(&evs, false);
        let chunked = write_session(&evs, true);
        let a = read_events(&legacy).unwrap();
        let b = read_events(&chunked).unwrap();
        assert_eq!(a.len(), b.len());
        assert_eq!(
            a.iter().map(|e| e.ts_mono_ns).collect::<Vec<_>>(),
            b.iter().map(|e| e.ts_mono_ns).collect::<Vec<_>>(),
            "order preserved"
        );
    }

    #[test]
    #[serial]
    fn filtered_equals_full_filter_at_boundaries() {
        let _home = temp_home();
        let evs: Vec<Event> = (0..2500u64)
            .map(|i| {
                ev(
                    i,
                    if i % 3 == 0 {
                        Source::Mark
                    } else {
                        Source::MetalHook
                    },
                )
            })
            .collect();
        let dir = write_session(&evs, true);
        for f in [
            EventFilter {
                source: Some(Source::Mark),
                from_ts: None,
                to_ts: None,
                payload_kind: None,
            },
            EventFilter {
                source: None,
                from_ts: Some(1000),
                to_ts: Some(1000),
                payload_kind: None,
            }, // single ts
            EventFilter {
                source: Some(Source::MetalHook),
                from_ts: Some(500),
                to_ts: Some(1500),
                payload_kind: None,
            },
            EventFilter {
                source: None,
                from_ts: Some(2000),
                to_ts: Some(10),
                payload_kind: None,
            }, // inverted → empty
        ] {
            let want: Vec<u64> = read_events(&dir)
                .unwrap()
                .into_iter()
                .filter(|e| f.matches(e))
                .map(|e| e.ts_mono_ns)
                .collect();
            let got: Vec<u64> = read_events_filtered(&dir, &f)
                .unwrap()
                .into_iter()
                .map(|e| e.ts_mono_ns)
                .collect();
            assert_eq!(got, want, "filter parity for {f:?}");
        }
    }

    #[test]
    #[serial]
    fn write_then_read_back() {
        let _home = temp_home();
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        for i in 0..5 {
            w.write_event(&Event {
                ts_mono_ns: i * 100,
                ts_wall_ns: 0,
                session_id: Uuid::nil(),
                source: Source::Mark,
                pid: None,
                seq: i,
                payload: Payload::Mark {
                    label: format!("m-{i}"),
                    fields: Default::default(),
                },
            })
            .unwrap();
        }
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "2026-05-13T12:00:00Z".into()).unwrap();

        let events = read_events(&dir).unwrap();
        assert_eq!(events.len(), 5);
        assert_eq!(events[4].seq, 4);

        let meta = read_metadata(&dir).unwrap();
        assert_eq!(meta.session_id, id);
        assert_eq!(meta.exit_code, Some(0));

        let sessions = list_sessions().unwrap();
        assert_eq!(sessions.len(), 1);
    }

    /// #268: a session directory whose name does not end with the short id
    /// (renamed, copied, hand-built) must still be found by its id —
    /// `smeltr sessions show <dir>` resolved the dir to an id, then failed to
    /// find the dir back from it.
    #[test]
    #[serial]
    fn find_session_dir_falls_back_to_metadata() {
        let home = temp_home();
        let id = SessionId::new();
        let w = SessionWriter::create(SessionMetadata::now_starting(id)).unwrap();
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "x".into()).unwrap();
        let renamed = home.path().join("sessions").join("my-run");
        std::fs::rename(&dir, &renamed).unwrap();
        assert_eq!(find_session_dir(id).unwrap(), Some(renamed));
        assert_eq!(find_session_dir(SessionId::new()).unwrap(), None);
    }

    #[test]
    #[serial]
    fn empty_root_lists_nothing() {
        let _home = temp_home();
        assert!(list_sessions().unwrap().is_empty());
    }

    #[test]
    #[serial]
    fn reader_falls_back_to_legacy_cbor() {
        let _home = temp_home();
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let dir = crate::session::session_dir(&meta);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            crate::session::metadata_path(&dir),
            format!(
                "session_id = \"{}\"\nstarted_rfc3339 = \"2026-05-14T00:00:00Z\"\nhost = \"x\"\nargv = []\n",
                id
            ),
        )
        .unwrap();

        let mut buf = Vec::new();
        crate::codec::write_frame(
            &mut buf,
            &Event {
                ts_mono_ns: 1,
                ts_wall_ns: 0,
                session_id: Uuid::nil(),
                source: Source::Mark,
                pid: None,
                seq: 1,
                payload: Payload::Mark {
                    label: "legacy".into(),
                    fields: Default::default(),
                },
            },
        )
        .unwrap();
        std::fs::write(dir.join("events.cbor"), &buf).unwrap();

        let events = read_events(&dir).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].payload, Payload::Mark { ref label, .. } if label == "legacy"));
    }

    #[test]
    #[serial]
    fn read_metadata_parses_scoped_kind() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.kind = SessionKind::Scoped {
            pid: 9999,
            argv: vec!["python".into(), "infer.py".into()],
        };
        let writer = SessionWriter::create(meta.clone()).unwrap();
        let dir = writer.dir().to_path_buf();
        drop(writer);
        let parsed = read_metadata(&dir).unwrap();
        match parsed.kind {
            SessionKind::Scoped { pid, ref argv } => {
                assert_eq!(pid, 9999);
                assert_eq!(argv, &vec!["python".to_string(), "infer.py".to_string()]);
            }
            SessionKind::Ambient => panic!("expected Scoped, got Ambient"),
        }
    }
}

/// #268: damaged or truncated files must yield every event that is still
/// decodable, never a wrong value, and say that the data is partial.
#[cfg(test)]
mod damage_tests {
    use super::*;
    use crate::chunked::ChunkConfig;
    use crate::event::{Payload, Source};
    use crate::session::{events_path_zst, SessionId, SessionMetadata};
    use crate::writer::SessionWriter;
    use serial_test::serial;

    fn ev(i: u64) -> Event {
        Event {
            ts_mono_ns: i,
            ts_wall_ns: i,
            session_id: uuid::Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: i,
            payload: Payload::Mark {
                label: format!("mark-{i}-{}", i * 7919),
                fields: Default::default(),
            },
        }
    }

    /// `n` events, a flush every `every` (one legacy frame / one chunk each).
    fn session(n: u64, every: u64, chunked: bool, finalize: bool) -> (tempfile::TempDir, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let meta = SessionMetadata::now_starting(SessionId::new());
        let cfg = chunked.then(ChunkConfig::default);
        let mut w = SessionWriter::create_with_chunk_config(meta, cfg).unwrap();
        let dir = w.dir().to_path_buf();
        for i in 0..n {
            w.write_event(&ev(i)).unwrap();
            if i % every == every - 1 {
                w.flush().unwrap();
            }
        }
        if finalize {
            w.finalize(Some(0), "2026-09-27T00:00:00Z".into()).unwrap();
        } else {
            w.flush().unwrap();
        }
        (home, dir)
    }

    /// Returned events are, in order, a subset of the originals with
    /// identical content; anything missing is reported as damage.
    fn assert_honest(dir: &Path, n: u64, what: &str) -> usize {
        let (evs, damage) = read_events_checked(dir)
            .unwrap_or_else(|e| panic!("{what}: the readable part must be returned, got {e}"));
        let mut last = None;
        for e in &evs {
            assert_eq!(
                *e,
                ev(e.seq),
                "{what}: event {} decoded to a wrong value",
                e.seq
            );
            assert!(last < Some(e.seq), "{what}: out of order or duplicated");
            last = Some(e.seq);
        }
        if evs.len() as u64 != n {
            assert!(
                damage.is_some(),
                "{what}: {} of {n} events returned as if complete",
                evs.len()
            );
        }
        evs.len()
    }

    fn flip_every_byte(dir: &Path, n: u64, fmt: &str) {
        let path = events_path_zst(dir);
        let orig = std::fs::read(&path).unwrap();
        for p in 0..orig.len() {
            let mut bytes = orig.clone();
            bytes[p] ^= 0x10;
            std::fs::write(&path, &bytes).unwrap();
            if fmt.starts_with("chunked") && p == 3 {
                // The format version byte: indistinguishable from a file
                // written by a newer smeltr, which is an error — loud, not
                // silent.
                assert!(read_events_checked(dir).is_err());
                continue;
            }
            assert_honest(
                dir,
                n,
                &format!("{fmt}: byte {p} of {} flipped", orig.len()),
            );
        }
        std::fs::write(&path, &orig).unwrap();
    }

    #[test]
    #[serial]
    fn legacy_byte_flips_never_yield_wrong_or_silently_missing_events() {
        let (_h, dir) = session(120, 20, false, true);
        flip_every_byte(&dir, 120, "legacy");
    }

    #[test]
    #[serial]
    fn chunked_byte_flips_never_yield_wrong_or_silently_missing_events() {
        let (_h, dir) = session(120, 20, true, true);
        flip_every_byte(&dir, 120, "chunked+footer");
        let (_h, dir) = session(120, 20, true, false);
        flip_every_byte(&dir, 120, "chunked scan");
    }

    fn truncate_everywhere(dir: &Path, n: u64, fmt: &str) {
        let path = events_path_zst(dir);
        let orig = std::fs::read(&path).unwrap();
        for len in 0..orig.len() {
            std::fs::write(&path, &orig[..len]).unwrap();
            assert_honest(
                dir,
                n,
                &format!("{fmt}: truncated to {len} of {}", orig.len()),
            );
        }
        std::fs::write(&path, &orig).unwrap();
    }

    #[test]
    #[serial]
    fn truncation_is_reported_for_every_format() {
        let (_h, dir) = session(120, 20, false, true);
        truncate_everywhere(&dir, 120, "legacy");
        let (_h, dir) = session(120, 20, true, true);
        truncate_everywhere(&dir, 120, "chunked+footer");
    }

    /// A damaged frame or chunk in the middle costs that unit only: the
    /// units after it are still read.
    #[test]
    #[serial]
    fn damage_in_the_middle_costs_only_the_damaged_unit() {
        for (chunked, finalize, what) in [
            (false, true, "legacy"),
            (true, true, "chunked+footer"),
            (true, false, "chunked scan"),
        ] {
            let (_h, dir) = session(100, 20, chunked, finalize);
            let path = events_path_zst(&dir);
            let mut bytes = std::fs::read(&path).unwrap();
            let mid = bytes.len() / 2;
            for b in &mut bytes[mid..mid + 8] {
                *b ^= 0xFF;
            }
            std::fs::write(&path, &bytes).unwrap();
            let got = assert_honest(&dir, 100, what);
            assert!(got >= 60, "{what}: only {got} of 100 events kept");
            let (evs, _) = read_events_checked(&dir).unwrap();
            assert_eq!(evs.last().map(|e| e.seq), Some(99), "{what}: tail lost");
        }
    }

    /// Chunked with a footer: the chunk length is taken from the
    /// CRC-protected index, not from the unprotected prefix in the file.
    #[test]
    #[serial]
    fn footer_read_ignores_a_corrupt_length_prefix() {
        let (_h, dir) = session(100, 20, true, true);
        let path = events_path_zst(&dir);
        let mut f = File::open(&path).unwrap();
        let entries = crate::chunked::read_footer(&mut f).unwrap().unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let at = entries[1].offset as usize;
        bytes[at..at + 4].copy_from_slice(&7u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        let (evs, _) = read_events_checked(&dir).unwrap();
        assert_eq!(evs.len(), 100);
    }

    /// A pre-#268 legacy file: one zstd frame, no checksum. A flipped byte
    /// in the middle must still leave the events before it readable
    /// (the whole read used to fail with `cbor decode`).
    #[test]
    #[serial]
    fn old_single_frame_file_keeps_its_prefix() {
        let (_h, dir) = session(1, 1, false, true);
        let mut enc = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
        for i in 0..3000 {
            crate::codec::write_frame(&mut enc, &ev(i)).unwrap();
            if i % 100 == 99 {
                std::io::Write::flush(&mut enc).unwrap();
            }
        }
        let mut bytes = enc.finish().unwrap();
        let mid = bytes.len() / 2;
        for b in &mut bytes[mid..mid + 4] {
            *b ^= 0xFF;
        }
        std::fs::write(events_path_zst(&dir), &bytes).unwrap();
        let (evs, damage) = read_events_checked(&dir).unwrap();
        assert!(damage.is_some());
        assert!(
            evs.len() >= 1000,
            "only {} events before the damage",
            evs.len()
        );
    }

    /// Events the writer dropped (backlog full) are damage too, although
    /// the file itself is intact.
    #[test]
    #[serial]
    fn dropped_events_in_metadata_are_reported() {
        let (_h, dir) = session(10, 5, false, true);
        let mut meta = read_metadata(&dir).unwrap();
        meta.dropped_events = Some(42);
        crate::session::write_metadata(&dir, &meta).unwrap();
        let (evs, damage) = read_events_checked(&dir).unwrap();
        assert_eq!(evs.len(), 10);
        let damage = damage.expect("dropped events are damage");
        assert!(damage.to_string().contains("42"), "{damage}");
    }

    #[test]
    #[serial]
    fn intact_sessions_report_no_damage() {
        for (chunked, finalize) in [(false, true), (true, true), (true, false), (false, false)] {
            let (_h, dir) = session(100, 20, chunked, finalize);
            let (evs, damage) = read_events_checked(&dir).unwrap();
            assert_eq!(evs.len(), 100);
            assert_eq!(damage, None, "chunked={chunked} finalize={finalize}");
        }
    }
}

/// #271: parallel, streamed decoding must read exactly what the sequential
/// #268 reader read — same events, same damage.
#[cfg(test)]
mod stream_tests {
    use super::*;
    use crate::codec::read_frame;
    use crate::event::{Payload, Source};
    use crate::session::{events_path_zst, SessionId, SessionMetadata};
    use crate::writer::SessionWriter;
    use serial_test::serial;

    fn ev(i: u64) -> Event {
        Event {
            ts_mono_ns: i,
            ts_wall_ns: i,
            session_id: uuid::Uuid::nil(),
            source: if i.is_multiple_of(2) {
                Source::Mark
            } else {
                Source::MetalHook
            },
            pid: None,
            seq: i,
            payload: Payload::Mark {
                label: format!("m-{i}"),
                fields: Default::default(),
            },
        }
    }

    fn frames(evs: impl IntoIterator<Item = Event>) -> Vec<u8> {
        let mut buf = Vec::new();
        for e in evs {
            crate::codec::write_frame(&mut buf, &e).unwrap();
        }
        buf
    }

    /// The #268 frame loop, kept as the reference.
    fn reference_parse(data: &[u8], at: usize, out: &mut Vec<Event>, problems: &mut Vec<String>) {
        let mut cur = std::io::Cursor::new(data);
        loop {
            match read_frame::<_, Event>(&mut cur) {
                Ok(Some(e)) => out.push(e),
                Ok(None) => break,
                Err(e) => {
                    problems.push(format!(
                        "undecodable event data at byte {} of the frame at {at}: {e}",
                        cur.position()
                    ));
                    break;
                }
            }
        }
    }

    /// `data` through a `FrameParser`, in pieces of `piece` bytes.
    fn parse(data: &[u8], piece: usize) -> (Vec<Event>, Vec<String>) {
        let mut out = Vec::new();
        let mut problems = Vec::new();
        let mut f = |e| {
            out.push(e);
            ControlFlow::Continue(())
        };
        let mut sink = Sink {
            filter: None,
            f: &mut f,
            seen: 0,
            stopped: false,
        };
        let mut parser = FrameParser::new(7);
        for chunk in data.chunks(piece.max(1)) {
            parser.feed(chunk, &mut sink, &mut problems);
        }
        parser.finish(&mut sink, &mut problems);
        (out, problems)
    }

    fn reference(data: &[u8]) -> (Vec<Event>, Vec<String>) {
        let mut out = Vec::new();
        let mut problems = Vec::new();
        reference_parse(data, 7, &mut out, &mut problems);
        (out, problems)
    }

    /// Whole (parallel: 3000 frames in one piece) or in pieces that cut
    /// frames anywhere (sequential), the events come out complete and in
    /// order.
    #[test]
    fn frames_decode_in_order_whatever_the_pieces() {
        let data = frames((0..3000).map(ev));
        let want: Vec<Event> = (0..3000).map(ev).collect();
        for piece in [data.len(), 1 << 16, 4096, 97, 1] {
            let (got, problems) = parse(&data, piece);
            assert_eq!(got, want, "piece {piece}");
            assert!(problems.is_empty(), "piece {piece}: {problems:?}");
        }
    }

    /// Cut anywhere, flipped anywhere, or with an impossible frame length:
    /// same events, same problem text as the sequential reader.
    #[test]
    fn damaged_frames_read_as_the_sequential_reader_did() {
        let data = frames((0..40).map(ev));
        let mut cases: Vec<Vec<u8>> = (0..data.len()).map(|n| data[..n].to_vec()).collect();
        for p in 0..data.len() {
            let mut d = data.clone();
            d[p] ^= 0x5A;
            cases.push(d);
        }
        let mut huge = frames((0..3).map(ev));
        huge.extend_from_slice(&u32::MAX.to_le_bytes());
        huge.extend(frames((3..6).map(ev)));
        cases.push(huge);
        // Large enough for the parallel path, bad frame in the middle.
        let mut big = frames((0..3000).map(ev));
        big.extend_from_slice(&3u32.to_le_bytes());
        big.extend_from_slice(&[0xff, 0xff, 0xff]);
        big.extend(frames((3000..4000).map(ev)));
        cases.push(big);
        for (i, d) in cases.iter().enumerate() {
            let want = reference(d);
            for piece in [d.len(), 13, 1] {
                assert_eq!(parse(d, piece), want, "case {i}, piece {piece}");
            }
        }
    }

    /// The #268 reader for a legacy file, verbatim, as the reference for the
    /// streamed one.
    fn reference_legacy(buf: &[u8]) -> (Vec<Event>, Vec<String>) {
        use crate::chunked::{frame_has_checksum, ZSTD_MAGIC};
        let mut out = Vec::new();
        let mut problems = Vec::new();
        let mut pos = 0;
        while pos < buf.len() {
            let rest = &buf[pos..];
            let checksummed = frame_has_checksum(rest);
            let trusted = rest.starts_with(&ZSTD_MAGIC) && (checksummed || pos == 0);
            if !trusted {
                match (pos + 1..buf.len()).find(|&p| frame_has_checksum(&buf[p..])) {
                    Some(p) => {
                        problems.push(format!("bytes {pos}..{p} of the event file are unreadable"));
                        pos = p;
                        continue;
                    }
                    None => {
                        problems.push(format!(
                            "the last {} bytes of the event file are unreadable",
                            buf.len() - pos
                        ));
                        break;
                    }
                }
            }
            let size = zstd::zstd_safe::find_frame_compressed_size(rest)
                .ok()
                .filter(|&n| n > 0 && n <= rest.len());
            let (data, err) = decompress_frame(&rest[..size.unwrap_or(rest.len())]);
            if let (Some(n), None) = (size, &err) {
                reference_parse(&data, pos, &mut out, &mut problems);
                pos += n;
                continue;
            }
            if !checksummed {
                reference_parse(&data, pos, &mut out, &mut problems);
            }
            let what = match (size, err) {
                (Some(_), Some(e)) => format!("is damaged ({e})"),
                _ => "is incomplete (truncated or damaged)".to_string(),
            };
            problems.push(format!(
                "the zstd frame at byte {pos} {what}; {}",
                if checksummed {
                    "its events were skipped"
                } else {
                    "the events after the damage are lost"
                }
            ));
            match find_magic(buf, pos + 1) {
                Some(p) => pos = p,
                None => break,
            }
        }
        (out, problems)
    }

    fn legacy(buf: &[u8]) -> (Vec<Event>, Vec<String>) {
        let mut out = Vec::new();
        let mut problems = Vec::new();
        let mut f = |e| {
            out.push(e);
            ControlFlow::Continue(())
        };
        let mut sink = Sink {
            filter: None,
            f: &mut f,
            seen: 0,
            stopped: false,
        };
        read_legacy_zst(buf, &mut sink, &mut problems);
        (out, problems)
    }

    /// A pre-#268 file (one unchecked frame) and a #268 one (checksummed
    /// frames), each flipped and cut at every byte: the streamed reader
    /// returns what the whole-frame reader returned.
    #[test]
    fn legacy_files_read_as_the_whole_frame_reader_did() {
        let mut enc = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
        for i in 0..400 {
            crate::codec::write_frame(&mut enc, &ev(i)).unwrap();
            if i % 50 == 49 {
                std::io::Write::flush(&mut enc).unwrap();
            }
        }
        let old = enc.finish().unwrap();
        let mut new = Vec::new();
        for part in 0..4 {
            let mut enc = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
            enc.include_checksum(true).unwrap();
            for i in part * 30..part * 30 + 30 {
                crate::codec::write_frame(&mut enc, &ev(i)).unwrap();
            }
            new.extend(enc.finish().unwrap());
        }
        // Large enough for many decoder calls: damage at sampled positions.
        let mut enc = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
        for i in 0..40_000 {
            crate::codec::write_frame(&mut enc, &ev(i)).unwrap();
        }
        let big = enc.finish().unwrap();
        let (mut more, mut fewer) = (0, 0);
        for (what, file, stride) in [
            ("old", &old, 1),
            ("new", &new, 1),
            ("big", &big, big.len() / 40),
        ] {
            assert_eq!(legacy(file), reference_legacy(file), "{what}: intact");
            for p in (0..file.len()).step_by(stride) {
                let mut flipped = file.clone();
                flipped[p] ^= 0x10;
                for (how, d) in [("flipped", &flipped[..]), ("cut", &file[..p])] {
                    let (got, want) = (legacy(d), reference_legacy(d));
                    let case = format!("{what}: byte {p} {how}");
                    if what == "new" || want.1.iter().all(|p| !p.contains("zstd frame")) {
                        // Checksummed frames, or no zstd error: identical.
                        assert_eq!(got, want, "{case}");
                        continue;
                    }
                    // An unchecked frame that fails to decompress: what came
                    // out before the failure depends on how much each decoder
                    // call asked for. Both keep a correct prefix and report
                    // the same failure.
                    let n = got.0.len().min(want.0.len());
                    assert_eq!(got.0[..n], want.0[..n], "{case}");
                    assert_eq!(got.1.last(), want.1.last(), "{case}");
                    more += usize::from(got.0.len() > want.0.len());
                    fewer += usize::from(got.0.len() < want.0.len());
                }
            }
        }
        assert_eq!(
            fewer, 0,
            "the streamed reader never keeps less ({more} times more)"
        );
    }

    fn write_session(n: u64, chunked: bool) -> std::path::PathBuf {
        let meta = SessionMetadata::now_starting(SessionId::new());
        let cfg = chunked.then_some(crate::chunked::ChunkConfig {
            max_events: 64,
            max_bytes: crate::chunked::CHUNK_BYTES,
            flush_min_bytes: crate::chunked::FLUSH_MIN_BYTES,
        });
        let mut w = SessionWriter::create_with_chunk_config(meta, cfg).unwrap();
        for i in 0..n {
            w.write_event(&ev(i)).unwrap();
            if i % 100 == 99 {
                w.flush().unwrap();
            }
        }
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "2026-09-27T00:00:00Z".into()).unwrap();
        dir
    }

    /// The visitor stops as soon as it is told to, and never hands over an
    /// event that does not match the filter.
    #[test]
    #[serial]
    fn for_each_event_filters_and_stops_early() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        for chunked in [false, true] {
            let dir = write_session(2500, chunked);
            let filter = EventFilter {
                source: Some(Source::MetalHook),
                ..Default::default()
            };
            let mut seen = Vec::new();
            let damage = for_each_event(&dir, Some(&filter), |e| {
                seen.push(e.seq);
                if seen.len() == 10 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .unwrap();
            assert_eq!(damage, None, "chunked {chunked}");
            assert_eq!(
                seen,
                (0..10).map(|i| 2 * i + 1).collect::<Vec<_>>(),
                "chunked {chunked}"
            );
            assert_eq!(session_event_count(&dir).unwrap(), 2500);
        }
    }

    /// Damage found before the stop is still reported; a filtered pass does
    /// not take the events it skipped for missing ones.
    #[test]
    #[serial]
    fn a_visit_reports_damage_but_not_filtered_events_as_missing() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let dir = write_session(1000, false);
        let filter = EventFilter {
            source: Some(Source::Mark),
            ..Default::default()
        };
        let clean = for_each_event(&dir, Some(&filter), |_| ControlFlow::Continue(())).unwrap();
        assert_eq!(clean, None);

        let path = events_path_zst(&dir);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 10);
        std::fs::write(&path, &bytes).unwrap();
        let (all, full) = read_events_checked(&dir).unwrap();
        assert!(all.len() < 1000 && full.is_some());
        let mut n = 0;
        let partial = for_each_event(&dir, None, |_| {
            n += 1;
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(
            partial, full,
            "a complete pass reports what read_events_checked does"
        );
        assert_eq!(n, all.len());
    }
}

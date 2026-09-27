//! Sequential session reader. Used by `smeltr sessions show` and replay.

use crate::chunked::ChunkIndexEntry;
use crate::codec::read_frame;
use crate::event::Event;
use crate::filter::EventFilter;
use crate::session::{metadata_path, sessions_root, SessionId, SessionMetadata};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
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
    let path = crate::session::events_path_for_read(dir);
    let buf = std::fs::read(&path)?;
    let mut problems = Vec::new();
    let events = if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        match crate::chunked::detect_bytes(&buf) {
            crate::chunked::Format::Chunked => read_chunked(&buf, &mut problems),
            crate::chunked::Format::Unsupported(v) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("session written by a newer smeltr (format v{v}); upgrade to read it"),
                ));
            }
            crate::chunked::Format::Legacy => read_legacy_zst(&buf, &mut problems),
        }
    } else {
        // Uncompressed .cbor from the first releases.
        let mut out = Vec::new();
        parse_frames(&buf, 0, &mut out, &mut problems);
        out
    };
    if let Ok(meta) = read_metadata(dir) {
        if let Some(written) = meta.event_count {
            let got = events.len() as u64;
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
    let damage = (!problems.is_empty()).then_some(Damage { problems });
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

/// Decode the event frames of `data` (decompressed from the unit at byte
/// `at`) into `out`; the events before the first undecodable byte are kept.
fn parse_frames(data: &[u8], at: usize, out: &mut Vec<Event>, problems: &mut Vec<String>) {
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

/// Decompress one zstd frame; on error, the output produced before it.
fn decompress_frame(frame: &[u8]) -> (Vec<u8>, Option<std::io::Error>) {
    let mut data = Vec::new();
    let res = zstd::stream::read::Decoder::with_buffer(frame)
        .map(|d| d.single_frame())
        .and_then(|mut d| d.read_to_end(&mut data));
    (data, res.err())
}

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
fn read_legacy_zst(buf: &[u8], problems: &mut Vec<String>) -> Vec<Event> {
    use crate::chunked::{frame_has_checksum, ZSTD_MAGIC};
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
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
        let (data, err) = decompress_frame(&rest[..size.unwrap_or(rest.len())]);
        if let (Some(n), None) = (size, &err) {
            parse_frames(&data, pos, &mut out, problems);
            pos += n;
            continue;
        }
        // Damaged or cut short. A checksummed frame's partial output cannot
        // be verified, so it is dropped whole.
        if !checksummed {
            parse_frames(&data, pos, &mut out, problems);
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
    out
}

/// A chunked events file: through its footer index when it has a valid one,
/// else by scanning.
fn read_chunked(buf: &[u8], problems: &mut Vec<String>) -> Vec<Event> {
    use crate::chunked::SessionFormatError;
    match crate::chunked::read_footer_with_offset(&mut std::io::Cursor::new(buf)) {
        Ok(Some((entries, footer_offset))) => {
            let mut fetch = |offset: u64, len: usize| -> std::io::Result<Vec<u8>> {
                let start = usize::try_from(offset).unwrap_or(usize::MAX);
                buf.get(start..start.saturating_add(len))
                    .map(<[u8]>::to_vec)
                    .ok_or_else(|| std::io::Error::other("chunk past the end of the file"))
            };
            decode_indexed(&mut fetch, &entries, footer_offset, None, problems)
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
    }
}

/// Decode the chunks a footer indexes. Lengths come from the CRC-protected
/// index — the `comp_len` prefix in the file is not protected. A chunk that
/// does not decode is skipped and reported; the others are still read (a
/// single bad chunk used to fail the whole read).
fn decode_indexed(
    fetch: &mut dyn FnMut(u64, usize) -> std::io::Result<Vec<u8>>,
    entries: &[ChunkIndexEntry],
    footer_offset: u64,
    filter: Option<&EventFilter>,
    problems: &mut Vec<String>,
) -> Vec<Event> {
    let mut out = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
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
        let decoded = fetch(entry.offset + 4, entry.comp_len as usize)
            .and_then(|bytes| crate::chunked::decode_chunk(&bytes));
        match decoded {
            Ok(events) => {
                if events.len() != entry.event_count as usize {
                    problems.push(format!(
                        "chunk {i}: {} events decoded, {} indexed",
                        events.len(),
                        entry.event_count
                    ));
                }
                match filter {
                    Some(f) => out.extend(events.into_iter().filter(|e| f.matches(e))),
                    None => out.extend(events),
                }
            }
            Err(e) => problems.push(format!(
                "chunk {i} ({} events) is unreadable: {e}",
                entry.event_count
            )),
        }
    }
    out
}

/// Like `read_events` but applies an `EventFilter`, using the chunk index to
/// skip irrelevant chunks when the session is sealed and chunked.
pub fn read_events_filtered(dir: &Path, filter: &EventFilter) -> std::io::Result<Vec<Event>> {
    let path = crate::session::events_path_for_read(dir);
    let mut f = File::open(&path)?;
    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        if let crate::chunked::Format::Chunked = crate::chunked::detect(&mut f)? {
            if let Ok(Some((entries, footer_offset))) =
                crate::chunked::read_footer_with_offset(&mut f)
            {
                let mut fetch = |offset: u64, len: usize| -> std::io::Result<Vec<u8>> {
                    f.seek(SeekFrom::Start(offset))?;
                    let mut buf = vec![0u8; len];
                    f.read_exact(&mut buf)?;
                    Ok(buf)
                };
                let mut problems = Vec::new();
                let out = decode_indexed(
                    &mut fetch,
                    &entries,
                    footer_offset,
                    Some(filter),
                    &mut problems,
                );
                if !problems.is_empty() {
                    tracing::warn!(dir = ?dir, "session data is incomplete: {}", problems.join("; "));
                }
                return Ok(out);
            }
        }
    }
    // Legacy / unsealed / corrupt: full read then filter.
    Ok(read_events(dir)?
        .into_iter()
        .filter(|e| filter.matches(e))
        .collect())
}

/// Return the total number of events in the session.
///
/// For sealed chunked sessions this is O(chunks) via the footer index;
/// otherwise it falls back to a full read.
pub fn session_event_count(dir: &Path) -> std::io::Result<usize> {
    let path = crate::session::events_path_for_read(dir);
    let mut f = File::open(&path)?;
    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        if let crate::chunked::Format::Chunked = crate::chunked::detect(&mut f)? {
            if let Ok(Some(entries)) = crate::chunked::read_footer(&mut f) {
                return Ok(entries.iter().map(|e| e.event_count as usize).sum());
            }
        }
    }
    Ok(read_events(dir)?.len())
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

//! Sequential session reader. Used by `smeltr sessions show` and replay.

use crate::chunked::ChunkIndexEntry;
use crate::codec::read_frame;
use crate::event::Event;
use crate::filter::EventFilter;
use crate::session::{metadata_path, sessions_root, SessionId, SessionMetadata};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
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

pub fn find_session_dir(id: SessionId) -> std::io::Result<Option<PathBuf>> {
    let short = id.short();
    for dir in list_sessions()? {
        if dir
            .file_name()
            .map(|n| n.to_string_lossy().ends_with(&short))
            .unwrap_or(false)
        {
            return Ok(Some(dir));
        }
    }
    Ok(None)
}

pub fn read_metadata(dir: &Path) -> std::io::Result<SessionMetadata> {
    let text = std::fs::read_to_string(metadata_path(dir))?;
    parse_metadata(&text).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "could not parse metadata.toml",
        )
    })
}

pub fn read_events(dir: &Path) -> std::io::Result<Vec<Event>> {
    let path = crate::session::events_path_for_read(dir);
    let mut f = File::open(&path)?;
    // Only the .zst path can be chunked; the legacy uncompressed .cbor never is.
    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        match crate::chunked::detect(&mut f)? {
            crate::chunked::Format::Chunked => match crate::chunked::read_footer(&mut f) {
                Ok(Some(entries)) => return read_chunked_from_footer(&mut f, &entries, None),
                Ok(None) => {
                    let mut f2 = File::open(&path)?;
                    return crate::chunked::scan_chunks(&mut f2);
                }
                Err(crate::chunked::SessionFormatError::FooterCorrupt(m)) => {
                    tracing::warn!(path=?path, "{m}; falling back to chunk scan");
                    let mut f2 = File::open(&path)?;
                    return crate::chunked::scan_chunks(&mut f2);
                }
                Err(crate::chunked::SessionFormatError::Io(e)) => return Err(e),
            },
            crate::chunked::Format::Unsupported(v) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("session written by a newer smeltr (format v{v}); upgrade to read it"),
                ));
            }
            crate::chunked::Format::Legacy => {
                f.seek(SeekFrom::Start(0))?;
                let mut r = zstd::stream::Decoder::new(f)?;
                return read_loop(&mut r, &path);
            }
        }
    }
    // legacy uncompressed
    let mut r = BufReader::new(f);
    read_loop(&mut r, &path)
}

/// Read all events from a sealed chunked session via its footer index.
///
/// Iterates entries in footer order (which preserves write order).  If a
/// `filter` is supplied, chunks whose `chunk_overlaps` returns `false` are
/// skipped entirely, and individual events that do not satisfy `matches` are
/// dropped.
fn read_chunked_from_footer(
    file: &mut File,
    entries: &[ChunkIndexEntry],
    filter: Option<&EventFilter>,
) -> std::io::Result<Vec<Event>> {
    let mut out = Vec::new();
    for entry in entries {
        // Skip chunks that cannot contain a matching event.
        if let Some(f) = filter {
            if !f.chunk_overlaps(entry) {
                continue;
            }
        }
        // Read the comp_len-prefixed compressed chunk bytes.
        file.seek(SeekFrom::Start(entry.offset))?;
        let mut len_buf = [0u8; 4];
        file.read_exact(&mut len_buf)?;
        let comp_len = u32::from_le_bytes(len_buf) as usize;
        let mut buf = vec![0u8; comp_len];
        file.read_exact(&mut buf)?;

        let events = crate::chunked::decode_chunk(&buf)?;
        if let Some(f) = filter {
            for ev in events {
                if f.matches(&ev) {
                    out.push(ev);
                }
            }
        } else {
            out.extend(events);
        }
    }
    Ok(out)
}

/// Like `read_events` but applies an `EventFilter`, using the chunk index to
/// skip irrelevant chunks when the session is sealed and chunked.
pub fn read_events_filtered(dir: &Path, filter: &EventFilter) -> std::io::Result<Vec<Event>> {
    let path = crate::session::events_path_for_read(dir);
    let mut f = File::open(&path)?;
    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        if let crate::chunked::Format::Chunked = crate::chunked::detect(&mut f)? {
            if let Ok(Some(entries)) = crate::chunked::read_footer(&mut f) {
                return read_chunked_from_footer(&mut f, &entries, Some(filter));
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

fn read_loop<R: std::io::Read>(r: &mut R, path: &Path) -> std::io::Result<Vec<Event>> {
    let mut out = Vec::new();
    loop {
        match read_frame::<_, Event>(r) {
            Ok(Some(e)) => out.push(e),
            Ok(None) => break,
            Err(crate::codec::CodecError::Truncated) => {
                tracing::warn!(path = ?path, "session truncated mid-frame, returning partial");
                break;
            }
            Err(crate::codec::CodecError::Io(io_err))
                if io_err.to_string().contains("incomplete frame") =>
            {
                // Zstd stream not sealed (mid-write flush). Treat as truncation.
                tracing::warn!(path = ?path, "zstd stream not sealed, returning partial");
                break;
            }
            Err(e) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                ))
            }
        }
    }
    Ok(out)
}

fn parse_metadata(text: &str) -> Option<SessionMetadata> {
    toml::from_str::<SessionMetadata>(text).ok()
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

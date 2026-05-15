//! Sequential session reader. Used by `smeltr sessions show` and replay.

use crate::codec::read_frame;
use crate::event::Event;
use crate::session::{metadata_path, sessions_root, SessionId, SessionMetadata};
use std::fs::File;
use std::io::BufReader;
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
    let f = File::open(&path)?;
    if path.extension().and_then(|e| e.to_str()) == Some("zst") {
        let mut r = zstd::stream::Decoder::new(f)?;
        read_loop(&mut r, &path)
    } else {
        let mut r = BufReader::new(f);
        read_loop(&mut r, &path)
    }
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
    use crate::event::{Payload, Source};
    use crate::session::{SessionKind, SessionMetadata};
    use crate::writer::SessionWriter;
    use serial_test::serial;
    use uuid::Uuid;

    fn temp_home() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", d.path());
        d
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
                },
            },
        )
        .unwrap();
        std::fs::write(dir.join("events.cbor"), &buf).unwrap();

        let events = read_events(&dir).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0].payload, Payload::Mark { ref label } if label == "legacy"));
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

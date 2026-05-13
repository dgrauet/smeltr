//! Active session bookkeeping inside the daemon.
//!
//! The daemon always has exactly one "active session" open. When events come
//! in, they're stamped with that session id and appended to its writer. On
//! shutdown the active session is finalized.

use smeltr_core::clock::MonoClock;
use smeltr_core::event::{Event, Payload, Source};
use smeltr_core::session::{SessionId, SessionMetadata};
use smeltr_core::writer::SessionWriter;
use std::sync::Mutex;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

pub struct ActiveSession {
    inner: Mutex<Inner>,
}

struct Inner {
    writer: SessionWriter,
    session_id: SessionId,
    clock: MonoClock,
    wall_epoch_ns: u64,
    seq: u64,
}

impl ActiveSession {
    pub fn open_new() -> std::io::Result<Self> {
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let writer = SessionWriter::create(meta)?;
        let clock = MonoClock::new();
        let wall_epoch_ns = now_unix_ns();
        let s = Self {
            inner: Mutex::new(Inner {
                writer,
                session_id: id,
                clock,
                wall_epoch_ns,
                seq: 0,
            }),
        };
        s.append_internal(
            Source::System,
            None,
            Payload::SessionStarted {
                wall_unix_ns: wall_epoch_ns,
            },
        )?;
        Ok(s)
    }

    pub fn id(&self) -> SessionId {
        self.inner.lock().unwrap().session_id
    }

    /// Stamps an event with current clock/seq/session id and writes it.
    /// Returns the fully-populated Event so it can also be broadcast.
    pub fn append(
        &self,
        source: Source,
        pid: Option<u32>,
        payload: Payload,
    ) -> std::io::Result<Event> {
        self.append_internal(source, pid, payload)
    }

    fn append_internal(
        &self,
        source: Source,
        pid: Option<u32>,
        payload: Payload,
    ) -> std::io::Result<Event> {
        let mut inner = self.inner.lock().unwrap();
        let ts_mono = inner.clock.now_ns();
        let ts_wall = inner.wall_epoch_ns + ts_mono;
        inner.seq += 1;
        let ev = Event {
            ts_mono_ns: ts_mono,
            ts_wall_ns: ts_wall,
            session_id: inner.session_id.0,
            source,
            pid,
            seq: inner.seq,
            payload,
        };
        inner
            .writer
            .write_event(&ev)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(ev)
    }

    pub fn finalize(self, exit_code: Option<i32>, reason: &str) -> std::io::Result<()> {
        let _ = self.append(
            Source::System,
            None,
            Payload::SessionEnded {
                wall_unix_ns: now_unix_ns(),
                reason: reason.to_string(),
            },
        );
        let inner = self.inner.into_inner().unwrap();
        let ended = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
        inner.writer.finalize(exit_code, ended)
    }
}

#[allow(dead_code)]
fn _suppress_unused_uuid(_u: Uuid) {}

fn now_unix_ns() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    now.as_secs() * 1_000_000_000 + (now.subsec_nanos() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::reader::{list_sessions, read_events, read_metadata};

    fn temp_home() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", d.path());
        d
    }

    #[test]
    fn session_lifecycle_appends_start_and_end() {
        let _home = temp_home();
        let s = ActiveSession::open_new().unwrap();
        s.append(Source::Mark, None, Payload::Mark { label: "hi".into() })
            .unwrap();
        s.finalize(Some(0), "test").unwrap();

        let dirs = list_sessions().unwrap();
        assert_eq!(dirs.len(), 1);
        let events = read_events(&dirs[0]).unwrap();
        // Expected: SessionStarted, Mark, SessionEnded
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0].payload, Payload::SessionStarted { .. }));
        assert!(matches!(events[1].payload, Payload::Mark { .. }));
        assert!(matches!(events[2].payload, Payload::SessionEnded { .. }));

        let meta = read_metadata(&dirs[0]).unwrap();
        assert_eq!(meta.exit_code, Some(0));
        assert!(meta.ended_rfc3339.is_some());
    }
}

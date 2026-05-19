//! Active session bookkeeping inside the daemon.

use crate::bus::Bus;
use crate::flight_recorder::FlightRecorder;
use smeltr_core::clock::MonoClock;
use smeltr_core::event::{Event, Payload, Source};
use smeltr_core::session::{SessionId, SessionMetadata};
use smeltr_core::writer::SessionWriter;
use std::sync::{Arc, Mutex};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

pub struct ActiveSession {
    inner: Mutex<Option<Inner>>,
    flight_recorder: Option<Arc<FlightRecorder>>,
    bus: Option<Bus>,
    scope_token: Option<String>,
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
        Self::open_new_full(None, None)
    }

    pub fn open_new_with_recorder(
        flight_recorder: Option<Arc<FlightRecorder>>,
    ) -> std::io::Result<Self> {
        Self::open_new_full(flight_recorder, None)
    }

    pub fn open_new_full(
        flight_recorder: Option<Arc<FlightRecorder>>,
        bus: Option<Bus>,
    ) -> std::io::Result<Self> {
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let writer = SessionWriter::create(meta)?;
        let clock = MonoClock::new();
        let wall_epoch_ns = now_unix_ns();
        let s = Self {
            inner: Mutex::new(Some(Inner {
                writer,
                session_id: id,
                clock,
                wall_epoch_ns,
                seq: 0,
            })),
            flight_recorder,
            bus,
            scope_token: None,
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

    pub fn open_scoped(
        pid: u32,
        argv: Vec<String>,
        scope_token: Option<String>,
        flight_recorder: Option<Arc<FlightRecorder>>,
        bus: Option<Bus>,
    ) -> std::io::Result<Self> {
        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.kind = smeltr_core::session::SessionKind::Scoped { pid, argv };
        meta.scope_token = scope_token.clone();
        let writer = SessionWriter::create(meta)?;
        let clock = MonoClock::new();
        let wall_epoch_ns = now_unix_ns();
        let s = Self {
            inner: Mutex::new(Some(Inner {
                writer,
                session_id: id,
                clock,
                wall_epoch_ns,
                seq: 0,
            })),
            flight_recorder,
            bus,
            scope_token,
        };
        s.append_internal(
            Source::System,
            Some(pid),
            Payload::SessionStarted {
                wall_unix_ns: wall_epoch_ns,
            },
        )?;
        Ok(s)
    }

    pub fn id(&self) -> SessionId {
        self.inner
            .lock()
            .unwrap()
            .as_ref()
            .expect("session not finalized")
            .session_id
    }

    pub fn scope_token(&self) -> Option<&str> {
        self.scope_token.as_deref()
    }

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
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| std::io::Error::other("session already finalized"))?;
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
        drop(guard);
        if let Some(fr) = &self.flight_recorder {
            fr.push(ev.clone());
        }
        if let Some(b) = &self.bus {
            b.publish(ev.clone());
        }
        Ok(ev)
    }

    /// Flushes the writer if the session is still active. Idempotent.
    pub fn flush(&self) -> std::io::Result<()> {
        let mut guard = self.inner.lock().unwrap();
        match guard.as_mut() {
            Some(inner) => inner.writer.flush(),
            None => Ok(()),
        }
    }

    /// Idempotent. Subsequent calls are no-ops.
    pub fn finalize(&self, exit_code: Option<i32>, reason: &str) -> std::io::Result<()> {
        let _ = self.append(
            Source::System,
            None,
            Payload::SessionEnded {
                wall_unix_ns: now_unix_ns(),
                reason: reason.to_string(),
            },
        );
        let inner = {
            let mut guard = self.inner.lock().unwrap();
            guard.take()
        };
        let Some(inner) = inner else { return Ok(()) };
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
    use serial_test::serial;
    use smeltr_core::reader::{list_sessions, read_events, read_metadata};

    fn temp_home() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", d.path());
        d
    }

    #[test]
    #[serial]
    fn open_scoped_writes_scoped_metadata() {
        let _home = temp_home();
        let s = ActiveSession::open_scoped(
            1234,
            vec!["python".into(), "x.py".into()],
            None,
            None,
            None,
        )
        .unwrap();
        s.finalize(Some(0), "test").unwrap();
        let dirs = list_sessions().unwrap();
        let meta = read_metadata(&dirs[0]).unwrap();
        match meta.kind {
            smeltr_core::session::SessionKind::Scoped { pid, argv } => {
                assert_eq!(pid, 1234);
                assert_eq!(argv, vec!["python".to_string(), "x.py".to_string()]);
            }
            other => panic!("expected Scoped, got {other:?}"),
        }
        let evs = read_events(&dirs[0]).unwrap();
        let started = evs
            .iter()
            .find(|e| matches!(e.payload, Payload::SessionStarted { .. }))
            .expect("SessionStarted event must exist");
        assert_eq!(
            started.pid,
            Some(1234),
            "SessionStarted must carry pid=Some(pid)"
        );
    }

    #[test]
    #[serial]
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

    #[test]
    #[serial]
    fn finalize_works_even_with_outstanding_arc_clones() {
        let _home = temp_home();
        let s = std::sync::Arc::new(ActiveSession::open_new().unwrap());
        let s_clone = s.clone();

        s.finalize(Some(7), "shutdown").unwrap();

        let dirs = list_sessions().unwrap();
        let meta = read_metadata(&dirs[0]).unwrap();
        assert_eq!(meta.exit_code, Some(7));
        assert!(meta.ended_rfc3339.is_some());
        drop(s_clone);
    }

    #[test]
    #[serial]
    fn open_scoped_persists_scope_token() {
        let _h = temp_home();
        let s = ActiveSession::open_scoped(
            4242,
            vec!["python".into()],
            Some("tok-XYZ".into()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(s.scope_token(), Some("tok-XYZ"));
        s.finalize(Some(0), "test").unwrap();

        // Re-read metadata from disk and check scope_token survived.
        let dirs = smeltr_core::reader::list_sessions().unwrap();
        let meta = smeltr_core::reader::read_metadata(&dirs[0]).unwrap();
        assert_eq!(meta.scope_token.as_deref(), Some("tok-XYZ"));
    }

    #[test]
    #[serial]
    fn open_scoped_without_token_is_none() {
        let _h = temp_home();
        let s = ActiveSession::open_scoped(7, vec!["x".into()], None, None, None).unwrap();
        assert!(s.scope_token().is_none());
    }

    #[test]
    #[serial]
    fn append_pushes_to_flight_recorder() {
        let _home = temp_home();
        let fr = std::sync::Arc::new(crate::flight_recorder::FlightRecorder::new(
            std::time::Duration::from_secs(60),
        ));
        let s = ActiveSession::open_new_with_recorder(Some(fr.clone())).unwrap();
        s.append(Source::Mark, None, Payload::Mark { label: "x".into() })
            .unwrap();
        // SessionStarted (emitted by constructor) + Mark = 2 events in the ring.
        assert_eq!(fr.len(), 2);
        s.finalize(Some(0), "test").unwrap();
    }
}

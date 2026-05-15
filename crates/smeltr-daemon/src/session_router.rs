//! Routes events to the correct ActiveSession: ambient (catch-all) or
//! one of the per-PID scoped sessions opened by `smeltr record`.

use crate::bus::Bus;
use crate::flight_recorder::FlightRecorder;
use crate::sessions::ActiveSession;
use smeltr_core::event::{Payload, Source};
use smeltr_core::session::SessionId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct SessionRouter {
    ambient: Arc<ActiveSession>,
    scoped: Mutex<HashMap<u32, Arc<ActiveSession>>>,
    flight_recorder: Option<Arc<FlightRecorder>>,
    bus: Option<Bus>,
}

impl SessionRouter {
    pub fn new(
        ambient: Arc<ActiveSession>,
        flight_recorder: Option<Arc<FlightRecorder>>,
        bus: Option<Bus>,
    ) -> Self {
        Self {
            ambient,
            scoped: Mutex::new(HashMap::new()),
            flight_recorder,
            bus,
        }
    }

    /// Append an Emit event. Routes by PID to the scoped session if one
    /// exists for that PID; otherwise to the ambient session.
    pub fn append(
        &self,
        source: Source,
        pid: Option<u32>,
        payload: Payload,
    ) -> std::io::Result<()> {
        let target = self.route_for_pid(pid);
        target.append(source, pid, payload).map(|_| ())
    }

    fn route_for_pid(&self, pid: Option<u32>) -> Arc<ActiveSession> {
        if let Some(p) = pid {
            let guard = self.scoped.lock().unwrap();
            if let Some(s) = guard.get(&p) {
                return s.clone();
            }
        }
        self.ambient.clone()
    }

    /// Open a scoped session for `pid`. If one already exists for this PID
    /// (e.g. duplicate AttachScopedProbes), the previous one is finalized
    /// with reason "superseded" to avoid leaking state.
    pub fn attach_scoped(&self, pid: u32, argv: Vec<String>) -> std::io::Result<SessionId> {
        let new = Arc::new(ActiveSession::open_scoped(
            pid,
            argv,
            self.flight_recorder.clone(),
            self.bus.clone(),
        )?);
        let id = new.id();
        let prev = {
            let mut guard = self.scoped.lock().unwrap();
            guard.insert(pid, new)
        };
        if let Some(prev) = prev {
            let _ = prev.finalize(None, "superseded by new AttachScopedProbes");
        }
        Ok(id)
    }

    /// Finalize a scoped session for `pid`. Returns the SessionId so callers
    /// can log it. None if no scoped session existed (idempotent).
    pub fn detach_scoped(&self, pid: u32, exit_code: Option<i32>) -> Option<SessionId> {
        let removed = {
            let mut guard = self.scoped.lock().unwrap();
            guard.remove(&pid)
        }?;
        let id = removed.id();
        let _ = removed.finalize(exit_code, &format!("record:exit pid={pid}"));
        Some(id)
    }

    pub fn ambient_id(&self) -> SessionId {
        self.ambient.id()
    }

    /// Flush every active session. Called on daemon shutdown.
    pub fn flush_all(&self) -> std::io::Result<()> {
        let guard = self.scoped.lock().unwrap();
        for s in guard.values() {
            s.flush()?;
        }
        drop(guard);
        self.ambient.flush()
    }

    /// Finalize every active session. Called on graceful daemon shutdown.
    pub fn finalize_all(&self, reason: &str) -> std::io::Result<()> {
        let scoped: Vec<Arc<ActiveSession>> = {
            let mut guard = self.scoped.lock().unwrap();
            guard.drain().map(|(_, v)| v).collect()
        };
        for s in scoped {
            let _ = s.finalize(None, reason);
        }
        self.ambient.finalize(None, reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use smeltr_core::reader::{list_sessions, read_events, read_metadata};
    use smeltr_core::session::SessionKind;

    fn temp_home() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", d.path());
        d
    }

    #[test]
    #[serial]
    fn pidless_event_goes_to_ambient() {
        let _h = temp_home();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let r = SessionRouter::new(ambient.clone(), None, None);
        r.append(
            Source::Mark,
            None,
            Payload::Mark {
                label: "ambient".into(),
            },
        )
        .unwrap();
        ambient.finalize(Some(0), "test").unwrap();
        let dirs = list_sessions().unwrap();
        assert_eq!(dirs.len(), 1);
        let evs = read_events(&dirs[0]).unwrap();
        assert!(evs.iter().any(|e| matches!(
            &e.payload,
            Payload::Mark { label } if label == "ambient"
        )));
    }

    #[test]
    #[serial]
    fn pid_event_routes_to_scoped_when_attached() {
        let _h = temp_home();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let r = SessionRouter::new(ambient.clone(), None, None);
        let scoped_id = r.attach_scoped(42, vec!["py".into(), "x".into()]).unwrap();
        r.append(
            Source::Mark,
            Some(42),
            Payload::Mark {
                label: "scoped".into(),
            },
        )
        .unwrap();
        r.detach_scoped(42, Some(0));
        ambient.finalize(Some(0), "test").unwrap();

        let dirs = list_sessions().unwrap();
        assert_eq!(dirs.len(), 2);
        let mut found_in_scoped = false;
        let mut found_in_ambient = false;
        for d in &dirs {
            let meta = read_metadata(d).unwrap();
            let evs = read_events(d).unwrap();
            let has_mark = evs.iter().any(|e| {
                matches!(
                    &e.payload,
                    Payload::Mark { label } if label == "scoped"
                )
            });
            match meta.kind {
                SessionKind::Scoped { pid, .. } => {
                    assert_eq!(pid, 42);
                    assert_eq!(meta.session_id, scoped_id);
                    if has_mark {
                        found_in_scoped = true;
                    }
                }
                SessionKind::Ambient => {
                    if has_mark {
                        found_in_ambient = true;
                    }
                }
            }
        }
        assert!(found_in_scoped, "scoped session must contain the Mark");
        assert!(
            !found_in_ambient,
            "ambient session must NOT contain the Mark"
        );
    }

    #[test]
    #[serial]
    fn pid_event_falls_back_to_ambient_when_unknown() {
        let _h = temp_home();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let r = SessionRouter::new(ambient.clone(), None, None);
        // No attach_scoped for pid 99 → should land in ambient.
        r.append(
            Source::Mark,
            Some(99),
            Payload::Mark {
                label: "fallback".into(),
            },
        )
        .unwrap();
        ambient.finalize(Some(0), "test").unwrap();

        let dirs = list_sessions().unwrap();
        assert_eq!(dirs.len(), 1);
        let evs = read_events(&dirs[0]).unwrap();
        assert!(evs.iter().any(|e| matches!(
            &e.payload,
            Payload::Mark { label } if label == "fallback"
        )));
    }

    #[test]
    #[serial]
    fn duplicate_attach_supersedes_previous() {
        let _h = temp_home();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let r = SessionRouter::new(ambient.clone(), None, None);
        let _id1 = r.attach_scoped(7, vec!["a".into()]).unwrap();
        let id2 = r.attach_scoped(7, vec!["b".into()]).unwrap();
        // Append goes to the SECOND scoped session.
        r.append(
            Source::Mark,
            Some(7),
            Payload::Mark {
                label: "after-supersede".into(),
            },
        )
        .unwrap();
        r.detach_scoped(7, Some(0));
        ambient.finalize(Some(0), "test").unwrap();

        // Three sessions: 2 scoped (one superseded, one finalized after Mark) + 1 ambient.
        let dirs = list_sessions().unwrap();
        assert_eq!(dirs.len(), 3);
        // The Mark must live exactly in the session with id2.
        for d in &dirs {
            let meta = read_metadata(d).unwrap();
            let evs = read_events(d).unwrap();
            let has = evs.iter().any(|e| {
                matches!(
                    &e.payload,
                    Payload::Mark { label } if label == "after-supersede"
                )
            });
            if has {
                assert_eq!(meta.session_id, id2);
            }
        }
    }
}

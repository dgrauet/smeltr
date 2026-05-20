//! Routes events to the correct ActiveSession: ambient (catch-all), or
//! one of the per-token/per-PID scoped sessions opened by `smeltr record`.
//!
//! Routing order: scope_token match -> PID match -> ambient fallback.
//! The token wins because it survives launcher processes (uv run, poetry,
//! python -m foo, shell wrappers) where the child PID diverges from the
//! process that actually emits the events.

use crate::bus::Bus;
use crate::flight_recorder::FlightRecorder;
use crate::sessions::ActiveSession;
use smeltr_core::event::{Payload, Source};
use smeltr_core::session::SessionId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct SessionRouter {
    ambient: Arc<ActiveSession>,
    by_pid: Mutex<HashMap<u32, Arc<ActiveSession>>>,
    by_token: Mutex<HashMap<String, Arc<ActiveSession>>>,
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
            by_pid: Mutex::new(HashMap::new()),
            by_token: Mutex::new(HashMap::new()),
            flight_recorder,
            bus,
        }
    }

    pub fn append(
        &self,
        source: Source,
        pid: Option<u32>,
        scope_token: Option<&str>,
        payload: Payload,
    ) -> std::io::Result<()> {
        let target = self.route_for(scope_token, pid);
        target.append(source, pid, payload).map(|_| ())
    }

    fn route_for(&self, token: Option<&str>, pid: Option<u32>) -> Arc<ActiveSession> {
        if let Some(t) = token {
            if let Some(s) = self.by_token.lock().unwrap().get(t) {
                return s.clone();
            }
        }
        if let Some(p) = pid {
            if let Some(s) = self.by_pid.lock().unwrap().get(&p) {
                return s.clone();
            }
        }
        self.ambient.clone()
    }

    pub fn attach_scoped(
        &self,
        pid: u32,
        argv: Vec<String>,
        scope_token: Option<String>,
        name: Option<String>,
    ) -> std::io::Result<SessionId> {
        let new = Arc::new(ActiveSession::open_scoped(
            pid,
            argv,
            scope_token.clone(),
            name,
            self.flight_recorder.clone(),
            self.bus.clone(),
        )?);
        let id = new.id();

        let prev_pid = {
            let mut guard = self.by_pid.lock().unwrap();
            guard.insert(pid, new.clone())
        };
        if let Some(t) = &scope_token {
            self.by_token.lock().unwrap().insert(t.clone(), new.clone());
        }
        if let Some(prev) = prev_pid {
            if let Some(prev_tok) = prev.scope_token() {
                let mut g = self.by_token.lock().unwrap();
                if g.get(prev_tok).map(|s| s.id()) == Some(prev.id()) {
                    g.remove(prev_tok);
                }
            }
            let _ = prev.finalize(None, "superseded by new AttachScopedProbes");
        }
        Ok(id)
    }

    pub fn detach_scoped(&self, pid: u32, exit_code: Option<i32>) -> Option<SessionId> {
        let removed = {
            let mut guard = self.by_pid.lock().unwrap();
            guard.remove(&pid)
        }?;
        let id = removed.id();
        if let Some(tok) = removed.scope_token() {
            self.by_token.lock().unwrap().remove(tok);
        }
        let _ = removed.finalize(exit_code, &format!("record:exit pid={pid}"));
        Some(id)
    }

    pub fn ambient_id(&self) -> SessionId {
        self.ambient.id()
    }

    pub fn flush_all(&self) -> std::io::Result<()> {
        let guard = self.by_pid.lock().unwrap();
        for s in guard.values() {
            s.flush()?;
        }
        drop(guard);
        self.ambient.flush()
    }

    pub fn finalize_all(&self, reason: &str) -> std::io::Result<()> {
        let scoped: Vec<Arc<ActiveSession>> = {
            let mut g = self.by_pid.lock().unwrap();
            g.drain().map(|(_, v)| v).collect()
        };
        self.by_token.lock().unwrap().clear();
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
            None,
            Payload::Mark {
                label: "ambient".into(),
                fields: Default::default(),
            },
        )
        .unwrap();
        ambient.finalize(Some(0), "test").unwrap();
        let dirs = list_sessions().unwrap();
        assert_eq!(dirs.len(), 1);
        let evs = read_events(&dirs[0]).unwrap();
        assert!(evs.iter().any(|e| matches!(
            &e.payload,
            Payload::Mark { label, .. } if label == "ambient"
        )));
    }

    #[test]
    #[serial]
    fn pid_event_routes_to_scoped_when_attached() {
        let _h = temp_home();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let r = SessionRouter::new(ambient.clone(), None, None);
        let scoped_id = r
            .attach_scoped(42, vec!["py".into(), "x".into()], None, None)
            .unwrap();
        r.append(
            Source::Mark,
            Some(42),
            None,
            Payload::Mark {
                label: "scoped".into(),
                fields: Default::default(),
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
                    Payload::Mark { label, .. } if label == "scoped"
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
            None,
            Payload::Mark {
                label: "fallback".into(),
                fields: Default::default(),
            },
        )
        .unwrap();
        ambient.finalize(Some(0), "test").unwrap();

        let dirs = list_sessions().unwrap();
        assert_eq!(dirs.len(), 1);
        let evs = read_events(&dirs[0]).unwrap();
        assert!(evs.iter().any(|e| matches!(
            &e.payload,
            Payload::Mark { label, .. } if label == "fallback"
        )));
    }

    #[test]
    #[serial]
    fn duplicate_attach_supersedes_previous() {
        let _h = temp_home();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let r = SessionRouter::new(ambient.clone(), None, None);
        let _id1 = r.attach_scoped(7, vec!["a".into()], None, None).unwrap();
        let id2 = r.attach_scoped(7, vec!["b".into()], None, None).unwrap();
        // Append goes to the SECOND scoped session.
        r.append(
            Source::Mark,
            Some(7),
            None,
            Payload::Mark {
                label: "after-supersede".into(),
                fields: Default::default(),
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
                    Payload::Mark { label, .. } if label == "after-supersede"
                )
            });
            if has {
                assert_eq!(meta.session_id, id2);
            }
        }
    }

    #[test]
    #[serial]
    fn token_event_routes_to_token_session_when_attached() {
        let _h = temp_home();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let r = SessionRouter::new(ambient.clone(), None, None);
        let scoped_id = r
            .attach_scoped(42, vec!["py".into()], Some("TOK".into()), None)
            .unwrap();
        r.append(
            Source::Mark,
            Some(42),
            Some("TOK"),
            Payload::Mark {
                label: "t1".into(),
                fields: Default::default(),
            },
        )
        .unwrap();
        r.detach_scoped(42, Some(0));
        ambient.finalize(Some(0), "test").unwrap();

        let dirs = list_sessions().unwrap();
        let mut found_in_scoped = false;
        for d in &dirs {
            let meta = read_metadata(d).unwrap();
            let evs = read_events(d).unwrap();
            let has = evs
                .iter()
                .any(|e| matches!(&e.payload, Payload::Mark { label, .. } if label == "t1"));
            if meta.session_id == scoped_id {
                found_in_scoped = has;
            } else {
                assert!(!has, "ambient must not contain the Mark");
            }
        }
        assert!(found_in_scoped);
    }

    #[test]
    #[serial]
    fn token_takes_precedence_over_pid_mismatch() {
        // The "uv -> python grandchild" repro: scoped session is registered for
        // pid=42 with token=TOK, but the emit comes from pid=999 (grandchild)
        // and carries the same token. Routing by token must win.
        let _h = temp_home();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let r = SessionRouter::new(ambient.clone(), None, None);
        let scoped_id = r
            .attach_scoped(42, vec!["uv".into()], Some("TOK".into()), None)
            .unwrap();
        r.append(
            Source::Mark,
            Some(999),
            Some("TOK"),
            Payload::Mark {
                label: "from-grandchild".into(),
                fields: Default::default(),
            },
        )
        .unwrap();
        r.detach_scoped(42, Some(0));
        ambient.finalize(Some(0), "test").unwrap();

        let dirs = list_sessions().unwrap();
        let mut found_in_scoped = false;
        for d in &dirs {
            let meta = read_metadata(d).unwrap();
            let evs = read_events(d).unwrap();
            let has = evs.iter().any(
                |e| matches!(&e.payload, Payload::Mark { label, .. } if label == "from-grandchild"),
            );
            if meta.session_id == scoped_id {
                found_in_scoped = has;
            } else {
                assert!(!has, "ambient must not contain the Mark");
            }
        }
        assert!(found_in_scoped);
    }

    #[test]
    #[serial]
    fn pid_fallback_works_when_token_absent() {
        let _h = temp_home();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let r = SessionRouter::new(ambient.clone(), None, None);
        let scoped_id = r
            .attach_scoped(42, vec!["py".into()], Some("TOK".into()), None)
            .unwrap();
        // Emit without a token - must fall back to PID match.
        r.append(
            Source::Mark,
            Some(42),
            None,
            Payload::Mark {
                label: "pid-fallback".into(),
                fields: Default::default(),
            },
        )
        .unwrap();
        r.detach_scoped(42, Some(0));
        ambient.finalize(Some(0), "test").unwrap();

        let dirs = list_sessions().unwrap();
        let mut found = false;
        for d in &dirs {
            let meta = read_metadata(d).unwrap();
            let evs = read_events(d).unwrap();
            if meta.session_id == scoped_id {
                found = evs.iter().any(
                    |e| matches!(&e.payload, Payload::Mark { label, .. } if label == "pid-fallback"),
                );
            }
        }
        assert!(found);
    }

    #[test]
    #[serial]
    fn unknown_token_falls_back_to_ambient() {
        let _h = temp_home();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let r = SessionRouter::new(ambient.clone(), None, None);
        r.append(
            Source::Mark,
            None,
            Some("BOGUS"),
            Payload::Mark {
                label: "stray".into(),
                fields: Default::default(),
            },
        )
        .unwrap();
        ambient.finalize(Some(0), "test").unwrap();

        let dirs = list_sessions().unwrap();
        assert_eq!(dirs.len(), 1);
        let evs = read_events(&dirs[0]).unwrap();
        assert!(evs
            .iter()
            .any(|e| matches!(&e.payload, Payload::Mark { label, .. } if label == "stray")));
    }

    #[test]
    #[serial]
    fn detach_evicts_both_pid_and_token_maps() {
        let _h = temp_home();
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let r = SessionRouter::new(ambient.clone(), None, None);
        r.attach_scoped(42, vec!["py".into()], Some("TOK".into()), None)
            .unwrap();
        r.detach_scoped(42, Some(0));
        // After detach, emits with the token must fall back to ambient.
        r.append(
            Source::Mark,
            Some(42),
            Some("TOK"),
            Payload::Mark {
                label: "post-detach".into(),
                fields: Default::default(),
            },
        )
        .unwrap();
        ambient.finalize(Some(0), "test").unwrap();

        let dirs = list_sessions().unwrap();
        let mut in_ambient = false;
        for d in &dirs {
            let meta = read_metadata(d).unwrap();
            let evs = read_events(d).unwrap();
            let has = evs.iter().any(
                |e| matches!(&e.payload, Payload::Mark { label, .. } if label == "post-detach"),
            );
            if matches!(meta.kind, SessionKind::Ambient) && has {
                in_ambient = true;
            }
        }
        assert!(
            in_ambient,
            "post-detach Mark must land in ambient (both maps evicted)"
        );
    }

    #[test]
    #[serial]
    fn attach_scoped_with_name_persists_in_metadata() {
        let _h = temp_home();
        std::env::remove_var("SMELTR_SESSION_NAME");
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let r = SessionRouter::new(ambient.clone(), None, None);
        let scoped_id = r
            .attach_scoped(
                42,
                vec!["py".into()],
                Some("TOK".into()),
                Some("named-run".into()),
            )
            .unwrap();
        r.detach_scoped(42, Some(0));
        ambient.finalize(Some(0), "test").unwrap();

        let dirs = list_sessions().unwrap();
        let mut found = false;
        for d in &dirs {
            let meta = read_metadata(d).unwrap();
            if meta.session_id == scoped_id {
                assert_eq!(meta.name.as_deref(), Some("named-run"));
                found = true;
            }
        }
        assert!(found);
    }
}

//! Wire protocol between `smeltr` CLI clients and `smeltrd`.
//!
//! Every message is encoded as a length-prefixed CBOR frame (same codec as
//! sessions on disk), so the same `smeltr_core::codec::{read_frame,write_frame}`
//! functions apply.

use serde::{Deserialize, Serialize};
use smeltr_core::event::Event;
use smeltr_core::session::SessionId;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op")]
pub enum ClientToDaemon {
    /// Open a new connection. Client identifies itself for logs.
    Hello { client: String },
    /// Append an event to the current active session. Server fills in
    /// ts_mono/ts_wall/seq/session_id; client only specifies source/pid/payload.
    Emit {
        source: smeltr_core::event::Source,
        pid: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope_token: Option<String>,
        payload: smeltr_core::event::Payload,
    },
    /// Request a list of session directory names (basename only).
    ListSessions,
    /// Request all events of a given session.
    GetSession { id: SessionId },
    /// Ask the daemon to stop cleanly.
    Shutdown,
    /// Attach scoped probes (mach-exceptions, pid-filtered crash reports) to
    /// the given PID. The daemon also opens a Scoped session for this PID
    /// from this point until DetachScopedProbes finalizes it.
    AttachScopedProbes {
        pid: u32,
        #[serde(default)]
        argv: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// Per-session chunked-format request (#188): set by `smeltr record`
        /// when SMELTR_SESSION_INDEX=1 is in the CLIENT environment. The
        /// daemon-side env remains the global default.
        #[serde(default)]
        chunked: bool,
    },
    /// Detach scoped probes for the given PID and emit a final marker.
    DetachScopedProbes { pid: u32, exit_code: Option<i32> },
    /// Attach a metal-hook reader to drain the given ring file for the child PID.
    AttachMetalHook { pid: u32, ring_path: String },
    /// Detach the metal-hook reader and let final frames flush.
    DetachMetalHook { pid: u32 },
    /// Streaming subscription: server replies Ack, then pushes one
    /// EventNotification per event published on the bus until the client
    /// closes the connection.
    SubscribeEvents,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum DaemonToClient {
    Welcome {
        daemon_version: String,
        active_session: SessionId,
    },
    Ack,
    Error {
        message: String,
    },
    SessionList {
        dirs: Vec<String>,
    },
    SessionEvents {
        events: Vec<Event>,
        metadata: smeltr_core::session::SessionMetadata,
    },
    EventNotification {
        event: Event,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::codec::{read_frame, write_frame};

    #[test]
    fn attach_scoped_probes_decodes_legacy_without_argv() {
        // Old client sent just `{ "op": "AttachScopedProbes", "pid": 4242 }`.
        // New deserializer must accept it, defaulting argv to [].
        let legacy_cbor = {
            use ciborium::value::Value;
            let msg = Value::Map(vec![
                (
                    Value::Text("op".into()),
                    Value::Text("AttachScopedProbes".into()),
                ),
                (Value::Text("pid".into()), Value::Integer(4242_i64.into())),
            ]);
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&msg, &mut buf).unwrap();
            buf
        };
        let decoded: ClientToDaemon = ciborium::de::from_reader(&legacy_cbor[..]).unwrap();
        match decoded {
            ClientToDaemon::AttachScopedProbes { pid, argv, .. } => {
                assert_eq!(pid, 4242);
                assert!(argv.is_empty());
            }
            other => panic!("expected AttachScopedProbes, got {other:?}"),
        }
    }

    #[test]
    fn attach_scoped_probes_round_trips_argv() {
        let msg = ClientToDaemon::AttachScopedProbes {
            pid: 4242,
            argv: vec!["python".into(), "script.py".into()],
            scope_token: None,
            name: None,
            chunked: false,
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&msg, &mut buf).unwrap();
        let decoded: ClientToDaemon = ciborium::de::from_reader(&buf[..]).unwrap();
        assert_eq!(decoded, msg);
    }

    #[test]
    fn client_msg_round_trip() {
        let m = ClientToDaemon::Hello {
            client: "test".into(),
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &m).unwrap();
        let back: ClientToDaemon = read_frame(&mut &buf[..]).unwrap().unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn daemon_msg_round_trip() {
        let m = DaemonToClient::Ack;
        let mut buf = Vec::new();
        write_frame(&mut buf, &m).unwrap();
        let back: DaemonToClient = read_frame(&mut &buf[..]).unwrap().unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn subscribe_events_round_trip() {
        let m = ClientToDaemon::SubscribeEvents;
        let mut buf = Vec::new();
        write_frame(&mut buf, &m).unwrap();
        let back: ClientToDaemon = read_frame(&mut &buf[..]).unwrap().unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn event_notification_round_trip() {
        use smeltr_core::event::{Payload, Source};
        use uuid::Uuid;
        let ev = Event {
            ts_mono_ns: 1,
            ts_wall_ns: 2,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: 1,
            payload: Payload::Mark {
                label: "hi".into(),
                fields: Default::default(),
            },
        };
        let m = DaemonToClient::EventNotification { event: ev.clone() };
        let mut buf = Vec::new();
        write_frame(&mut buf, &m).unwrap();
        let back: DaemonToClient = read_frame(&mut &buf[..]).unwrap().unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn emit_decodes_legacy_without_scope_token() {
        // Old client sent Emit without scope_token. Payload uses `tag = "kind"`.
        let legacy_cbor = {
            use ciborium::value::Value;
            let msg = Value::Map(vec![
                (Value::Text("op".into()), Value::Text("Emit".into())),
                (Value::Text("source".into()), Value::Text("Mark".into())),
                (Value::Text("pid".into()), Value::Integer(4242_i64.into())),
                (
                    Value::Text("payload".into()),
                    Value::Map(vec![
                        (Value::Text("kind".into()), Value::Text("Mark".into())),
                        (Value::Text("label".into()), Value::Text("x".into())),
                    ]),
                ),
            ]);
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&msg, &mut buf).unwrap();
            buf
        };
        let decoded: ClientToDaemon = ciborium::de::from_reader(&legacy_cbor[..]).unwrap();
        match decoded {
            ClientToDaemon::Emit { scope_token, .. } => assert!(scope_token.is_none()),
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    #[test]
    fn emit_roundtrips_with_scope_token() {
        let msg = ClientToDaemon::Emit {
            source: smeltr_core::event::Source::Mark,
            pid: Some(7),
            scope_token: Some("tok-abc".into()),
            payload: smeltr_core::event::Payload::Mark {
                label: "y".into(),
                fields: Default::default(),
            },
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let back: ClientToDaemon = read_frame(&mut &buf[..]).unwrap().unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn attach_scoped_probes_decodes_legacy_without_name() {
        use ciborium::value::Value;
        // Simulate a message that has scope_token but no name field (legacy client).
        let legacy_cbor = {
            let msg = Value::Map(vec![
                (
                    Value::Text("op".into()),
                    Value::Text("AttachScopedProbes".into()),
                ),
                (Value::Text("pid".into()), Value::Integer(9001_i64.into())),
                (
                    Value::Text("argv".into()),
                    Value::Array(vec![
                        Value::Text("python".into()),
                        Value::Text("x.py".into()),
                    ]),
                ),
                (
                    Value::Text("scope_token".into()),
                    Value::Text("tok-T".into()),
                ),
            ]);
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&msg, &mut buf).unwrap();
            buf
        };
        let decoded: ClientToDaemon = ciborium::de::from_reader(&legacy_cbor[..]).unwrap();
        match decoded {
            ClientToDaemon::AttachScopedProbes {
                name, scope_token, ..
            } => {
                assert!(name.is_none());
                assert_eq!(scope_token.as_deref(), Some("tok-T"));
            }
            other => panic!("expected AttachScopedProbes, got {other:?}"),
        }
    }

    #[test]
    fn attach_scoped_probes_roundtrips_with_name() {
        use smeltr_core::codec::write_frame;
        let msg = ClientToDaemon::AttachScopedProbes {
            pid: 1234,
            argv: vec!["uv".into(), "run".into()],
            scope_token: Some("tok-T".into()),
            name: Some("ltx2-run".into()),
            chunked: false,
        };
        let mut buf = Vec::new();
        let mut writer = std::io::Cursor::new(&mut buf);
        write_frame(&mut writer, &msg).unwrap();
        let mut reader = std::io::Cursor::new(&buf);
        let back: ClientToDaemon = read_frame(&mut reader).unwrap().unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn attach_scoped_probes_decodes_legacy_without_scope_token() {
        let legacy_cbor = {
            use ciborium::value::Value;
            let msg = Value::Map(vec![
                (
                    Value::Text("op".into()),
                    Value::Text("AttachScopedProbes".into()),
                ),
                (Value::Text("pid".into()), Value::Integer(9001_i64.into())),
                (
                    Value::Text("argv".into()),
                    Value::Array(vec![
                        Value::Text("python".into()),
                        Value::Text("x.py".into()),
                    ]),
                ),
            ]);
            let mut buf = Vec::new();
            ciborium::ser::into_writer(&msg, &mut buf).unwrap();
            buf
        };
        let decoded: ClientToDaemon = ciborium::de::from_reader(&legacy_cbor[..]).unwrap();
        match decoded {
            ClientToDaemon::AttachScopedProbes { scope_token, .. } => {
                assert!(scope_token.is_none())
            }
            other => panic!("expected AttachScopedProbes, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod chunked_field_compat_tests {
    use super::*;

    /// #188: pre-existing clients omit `chunked` — the CBOR frame must
    /// decode with the serde default (false). Encoded via a shadow enum
    /// replicating the OLD wire shape.
    #[test]
    fn attach_without_chunked_field_decodes() {
        #[derive(serde::Serialize)]
        #[serde(tag = "op")]
        enum OldClientToDaemon {
            AttachScopedProbes { pid: u32, argv: Vec<String> },
        }
        let mut buf = Vec::new();
        ciborium::ser::into_writer(
            &OldClientToDaemon::AttachScopedProbes {
                pid: 7,
                argv: vec!["python".into()],
            },
            &mut buf,
        )
        .unwrap();
        let msg: ClientToDaemon = ciborium::de::from_reader(buf.as_slice()).unwrap();
        match msg {
            ClientToDaemon::AttachScopedProbes { chunked, pid, .. } => {
                assert_eq!(pid, 7);
                assert!(!chunked);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

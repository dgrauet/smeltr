//! Wire protocol between `smeltr` CLI clients and `smeltrd`.
//!
//! Every message is encoded as a length-prefixed CBOR frame (same codec as
//! sessions on disk), so the same `smeltr_core::codec::{read_frame,write_frame}`
//! functions apply.

use serde::{Deserialize, Serialize};
use smeltr_core::event::Event;
use smeltr_core::session::SessionId;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op")]
pub enum ClientToDaemon {
    /// Open a new connection. Client identifies itself for logs.
    Hello { client: String },
    /// Append an event to the current active session. Server fills in
    /// ts_mono/ts_wall/seq/session_id; client only specifies source/pid/payload.
    Emit {
        source: smeltr_core::event::Source,
        pid: Option<u32>,
        payload: smeltr_core::event::Payload,
    },
    /// Request a list of session directory names (basename only).
    ListSessions,
    /// Request all events of a given session.
    GetSession { id: SessionId },
    /// Ask the daemon to stop cleanly.
    Shutdown,
    /// Attach scoped probes (mach-exceptions, pid-filtered crash reports) to
    /// the given child PID. Used by `smeltr record`.
    AttachScopedProbes { pid: u32 },
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
            payload: Payload::Mark { label: "hi".into() },
        };
        let m = DaemonToClient::EventNotification { event: ev.clone() };
        let mut buf = Vec::new();
        write_frame(&mut buf, &m).unwrap();
        let back: DaemonToClient = read_frame(&mut &buf[..]).unwrap().unwrap();
        assert_eq!(m, back);
    }
}

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
    Emit { source: smeltr_core::event::Source,
           pid:    Option<u32>,
           payload: smeltr_core::event::Payload },
    /// Request a list of session directory names (basename only).
    ListSessions,
    /// Request all events of a given session.
    GetSession { id: SessionId },
    /// Ask the daemon to stop cleanly.
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind")]
pub enum DaemonToClient {
    Welcome { daemon_version: String, active_session: SessionId },
    Ack,
    Error { message: String },
    SessionList { dirs: Vec<String> },
    SessionEvents { events: Vec<Event>, metadata: smeltr_core::session::SessionMetadata },
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::codec::{read_frame, write_frame};

    #[test]
    fn client_msg_round_trip() {
        let m = ClientToDaemon::Hello { client: "test".into() };
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
}

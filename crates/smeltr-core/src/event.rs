//! Event model. All producers emit values of this type.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    Mark, // user marker
    System, // internal daemon events (session start/end, drops)
          // Probes added in later plans:
          // IoReport, Vm, Proc, OsLog, Thermal, MachExc, CrashReport, MetalHook, PythonSidecar
}

/// Tagged union of every payload smeltr can produce. We start with the two that
/// the foundation plan exercises end-to-end; new variants are added by later
/// plans. CBOR uses internally-tagged enums so adding variants is backward
/// compatible for readers that ignore unknown tags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Payload {
    Mark { label: String },
    SessionStarted { wall_unix_ns: u64 },
    SessionEnded { wall_unix_ns: u64, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub ts_mono_ns: u64,
    pub ts_wall_ns: u64,
    pub session_id: Uuid,
    pub source: Source,
    pub pid: Option<u32>,
    pub seq: u64,
    pub payload: Payload,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Event {
        Event {
            ts_mono_ns: 1_234_567,
            ts_wall_ns: 1_715_600_000_000_000_000,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: Some(42),
            seq: 7,
            payload: Payload::Mark {
                label: "hello".into(),
            },
        }
    }

    #[test]
    fn cbor_round_trip() {
        let e = sample();
        let mut buf = Vec::new();
        ciborium::into_writer(&e, &mut buf).unwrap();
        let decoded: Event = ciborium::from_reader(&buf[..]).unwrap();
        assert_eq!(e, decoded);
    }

    #[test]
    fn cbor_size_reasonable() {
        let e = sample();
        let mut buf = Vec::new();
        ciborium::into_writer(&e, &mut buf).unwrap();
        // sanity check: a mark event should encode in < 200 bytes
        assert!(buf.len() < 200, "actual size {}", buf.len());
    }
}

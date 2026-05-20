//! Internal broadcast bus. Every event written to disk is also broadcast to
//! subscribed clients (TUI, MCP, replay viewers). Backpressure: if a
//! subscriber lags behind by more than `CAPACITY` events, it is dropped from
//! its end (the receiver gets `RecvError::Lagged(n)`).

use smeltr_core::event::Event;
use tokio::sync::broadcast;

const CAPACITY: usize = 1024;

#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<Event>,
}

impl Bus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self { tx }
    }
    pub fn publish(&self, ev: Event) {
        // Sending to a broadcast channel with zero receivers returns Err — we
        // don't care, the event still got persisted by the writer.
        let _ = self.tx.send(ev);
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Payload, Source};
    use uuid::Uuid;

    fn ev(seq: u64) -> Event {
        Event {
            ts_mono_ns: 0,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq,
            payload: Payload::Mark {
                label: "x".into(),
                fields: Default::default(),
            },
        }
    }

    #[tokio::test]
    async fn publish_with_no_subscribers_is_ok() {
        let b = Bus::new();
        b.publish(ev(1)); // must not panic
    }

    #[tokio::test]
    async fn subscriber_receives_events() {
        let b = Bus::new();
        let mut rx = b.subscribe();
        b.publish(ev(7));
        let got = rx.recv().await.unwrap();
        assert_eq!(got.seq, 7);
    }
}

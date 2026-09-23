use smeltr_core::event::{Payload, Source};
use std::sync::Arc;

/// Type-erased way for a probe to emit events. The daemon implements this by
/// forwarding to ActiveSession::append + Bus::send.
pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, source: Source, pid: Option<u32>, payload: Payload);

    /// Emit an event that carries its own timestamp, on the
    /// `smeltr_core::clock::uptime_raw_ns` clock — for sources that record
    /// events before the daemon reads them (the Metal hook's ring). Stamping
    /// those at receipt put a whole ring drain on one instant (#244).
    fn emit_at(&self, source: Source, pid: Option<u32>, uptime_raw_ns: u64, payload: Payload) {
        let _ = uptime_raw_ns;
        self.emit(source, pid, payload);
    }
}

pub type SharedSink = Arc<dyn EventSink>;

pub mod test_util {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct CapturingSink {
        pub events: Mutex<Vec<(Source, Option<u32>, Payload)>>,
        /// The raw timestamp of each event, `None` when emitted without one.
        pub stamps: Mutex<Vec<Option<u64>>>,
    }

    impl EventSink for CapturingSink {
        fn emit(&self, source: Source, pid: Option<u32>, payload: Payload) {
            self.events.lock().unwrap().push((source, pid, payload));
            self.stamps.lock().unwrap().push(None);
        }

        fn emit_at(&self, source: Source, pid: Option<u32>, uptime_raw_ns: u64, payload: Payload) {
            self.events.lock().unwrap().push((source, pid, payload));
            self.stamps.lock().unwrap().push(Some(uptime_raw_ns));
        }
    }
}

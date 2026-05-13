use smeltr_core::event::{Payload, Source};
use std::sync::Arc;

/// Type-erased way for a probe to emit events. The daemon implements this by
/// forwarding to ActiveSession::append + Bus::send.
pub trait EventSink: Send + Sync + 'static {
    fn emit(&self, source: Source, pid: Option<u32>, payload: Payload);
}

pub type SharedSink = Arc<dyn EventSink>;

pub mod test_util {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    pub struct CapturingSink {
        pub events: Mutex<Vec<(Source, Option<u32>, Payload)>>,
    }

    impl EventSink for CapturingSink {
        fn emit(&self, source: Source, pid: Option<u32>, payload: Payload) {
            self.events.lock().unwrap().push((source, pid, payload));
        }
    }
}

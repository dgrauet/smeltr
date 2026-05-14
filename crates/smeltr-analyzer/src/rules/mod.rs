pub mod metal_error;
pub mod mlx_timing;
pub mod queue_depth;
pub mod queue_pressure;
pub mod system_pressure;

#[cfg(test)]
mod test_helpers {
    use smeltr_core::event::{Event, Payload, Source};
    use uuid::Uuid;

    pub fn ev(ts: u64, source: Source, payload: Payload) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source,
            pid: None,
            seq: ts,
            payload,
        }
    }
}

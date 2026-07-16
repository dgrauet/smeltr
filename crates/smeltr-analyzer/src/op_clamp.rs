//! Per-CB serialization clamp for op GPU times (#146).
//!
//! Per-op `gpu_ns` comes from stage-boundary counter sampling: each
//! encoder's `[begin, end]` GPU-timestamp window. Apple GPUs pipeline
//! adjacent encoders and command buffers, so those windows overlap and
//! summing them double-counts the overlap — measured at 121 % of the
//! scope wall on a MoE-heavy fp32 workload (Hunyuan3D stage 1).
//!
//! Ground truth per CB is its serialization-clamped execution window
//! (#125 semantics): `completed − max(scheduled, previous completion on
//! the same queue)`. When a CB's op sum exceeds that window, every op of
//! that CB is rescaled proportionally so the sum equals the window.
//! Ops are never scaled up.
//!
//! `cb_id` is a recycled Metal pointer (#127): windows are paired with
//! CbOps chronologically and consumed per lifetime, never indexed
//! session-wide.

use smeltr_core::event::{Event, Payload};
use std::collections::HashMap;

/// Scale factors for `MetalCbOps` events whose op sum exceeded the CB's
/// serialization-clamped window, keyed by event `seq`. Events absent from
/// the map are already physically consistent and keep their raw values.
pub struct OpTimeScales {
    scales: HashMap<u64, f64>,
}

impl OpTimeScales {
    /// Number of CBs whose op times were clamped.
    pub fn clamped_cb_count(&self) -> u64 {
        self.scales.len() as u64
    }

    /// Corrected gpu_ns for one op of the CbOps event with this `seq`.
    pub fn scaled_gpu_ns(&self, seq: u64, gpu_ns: u64) -> u64 {
        match self.scales.get(&seq) {
            Some(s) => (gpu_ns as f64 * s) as u64,
            None => gpu_ns,
        }
    }
}

/// Chronological read-only pass computing the per-CbOps-event scale
/// factors. Events must be in session order (as read from disk).
pub fn compute_op_time_scales(events: &[Event]) -> OpTimeScales {
    let mut last_sched: HashMap<u64, u64> = HashMap::new();
    let mut queue_last_done: HashMap<u64, u64> = HashMap::new();
    // cb_id -> serialization-clamped window of its latest completion,
    // consumed by the CbOps event that follows it (#127 pairing).
    let mut window_for_cb: HashMap<u64, u64> = HashMap::new();
    let mut scales: HashMap<u64, f64> = HashMap::new();

    for ev in events {
        match &ev.payload {
            Payload::MetalCbScheduled { cb_id, .. } => {
                last_sched.insert(*cb_id, ev.ts_mono_ns);
            }
            Payload::MetalCbCompleted {
                cb_id, queue_id, ..
            } => {
                let done = ev.ts_mono_ns;
                // No scheduled event seen (partial capture): skip the clamp
                // for this lifetime rather than guessing a window.
                if let Some(sched) = last_sched.remove(cb_id) {
                    let start = sched.max(queue_last_done.get(queue_id).copied().unwrap_or(0));
                    window_for_cb.insert(*cb_id, done.saturating_sub(start));
                } else {
                    window_for_cb.remove(cb_id);
                }
                queue_last_done.insert(*queue_id, done);
            }
            Payload::MetalCbOps { cb_id, ops } => {
                if let Some(window) = window_for_cb.remove(cb_id) {
                    let sum: u64 = ops.iter().map(|o| o.gpu_ns).sum();
                    if sum > window {
                        scales.insert(ev.seq, window as f64 / sum as f64);
                    }
                }
            }
            _ => {}
        }
    }
    OpTimeScales { scales }
}

/// Rewrites `MetalCbOps.ops[].gpu_ns` in place with the clamped values.
/// Used by consumers that own their event vector (breakdown); slice-based
/// consumers (origins) apply `scaled_gpu_ns` at read time instead.
pub fn apply_op_time_scales(events: &mut [Event], scales: &OpTimeScales) {
    for ev in events.iter_mut() {
        let seq = ev.seq;
        if let Payload::MetalCbOps { ops, .. } = &mut ev.payload {
            if scales.scales.contains_key(&seq) {
                for op in ops.iter_mut() {
                    op.gpu_ns = scales.scaled_gpu_ns(seq, op.gpu_ns);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{OpSample, Source};
    use uuid::Uuid;

    fn ev(seq: u64, ts: u64, payload: Payload) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq,
            payload,
        }
    }

    fn sched(seq: u64, ts: u64, cb_id: u64, queue_id: u64) -> Event {
        ev(seq, ts, Payload::MetalCbScheduled { cb_id, queue_id })
    }

    fn done(seq: u64, ts: u64, cb_id: u64, queue_id: u64) -> Event {
        ev(
            seq,
            ts,
            Payload::MetalCbCompleted {
                cb_id,
                queue_id,
                status: 4,
                error_code: None,
                error_domain: None,
                in_flight_ns: 0,
            },
        )
    }

    fn cbops(seq: u64, ts: u64, cb_id: u64, gpu_ns: &[u64]) -> Event {
        ev(
            seq,
            ts,
            Payload::MetalCbOps {
                cb_id,
                ops: gpu_ns
                    .iter()
                    .enumerate()
                    .map(|(i, ns)| OpSample {
                        name: format!("K_{i}"),
                        gpu_ns: *ns,
                        count: 1,
                        symbol: None,
                    })
                    .collect(),
            },
        )
    }

    fn op_sums(events: &[Event]) -> Vec<Vec<u64>> {
        events
            .iter()
            .filter_map(|e| match &e.payload {
                Payload::MetalCbOps { ops, .. } => Some(ops.iter().map(|o| o.gpu_ns).collect()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn ops_exceeding_window_are_rescaled_proportionally() {
        // window = 100 (sched 0 -> done 100); ops sum 400 -> scale 0.25.
        let mut events = vec![
            sched(1, 0, 1, 9),
            done(2, 100, 1, 9),
            cbops(3, 101, 1, &[300, 100]),
        ];
        let scales = compute_op_time_scales(&events);
        assert_eq!(scales.clamped_cb_count(), 1);
        apply_op_time_scales(&mut events, &scales);
        assert_eq!(op_sums(&events), vec![vec![75, 25]]);
    }

    #[test]
    fn ops_within_window_are_untouched() {
        let mut events = vec![
            sched(1, 0, 1, 9),
            done(2, 1000, 1, 9),
            cbops(3, 1001, 1, &[300, 100]),
        ];
        let scales = compute_op_time_scales(&events);
        assert_eq!(scales.clamped_cb_count(), 0);
        apply_op_time_scales(&mut events, &scales);
        assert_eq!(op_sums(&events), vec![vec![300, 100]]);
    }

    #[test]
    fn window_shrinks_behind_previous_completion_on_same_queue() {
        // CB 2 is scheduled at t=0 but the queue is busy with CB 1 until
        // t=900: its window is 1000-900=100, not 1000.
        let mut events = vec![
            sched(1, 0, 1, 9),
            sched(2, 0, 2, 9),
            done(3, 900, 1, 9),
            cbops(4, 901, 1, &[900]),
            done(5, 1000, 2, 9),
            cbops(6, 1001, 2, &[400]),
        ];
        let scales = compute_op_time_scales(&events);
        assert_eq!(scales.clamped_cb_count(), 1);
        apply_op_time_scales(&mut events, &scales);
        assert_eq!(op_sums(&events), vec![vec![900], vec![100]]);
    }

    #[test]
    fn distinct_queues_do_not_serialize_each_other() {
        let mut events = vec![
            sched(1, 0, 1, 9),
            sched(2, 0, 2, 8),
            done(3, 900, 1, 9),
            cbops(4, 901, 1, &[900]),
            done(5, 1000, 2, 8),
            cbops(6, 1001, 2, &[400]),
        ];
        let scales = compute_op_time_scales(&events);
        assert_eq!(scales.clamped_cb_count(), 0);
        apply_op_time_scales(&mut events, &scales);
        assert_eq!(op_sums(&events), vec![vec![900], vec![400]]);
    }

    #[test]
    fn recycled_cb_id_pairs_windows_chronologically() {
        // Same cb pointer, two lifetimes: first window 100 (clamps 400->100),
        // second window 1000 (no clamp for sum 400).
        let mut events = vec![
            sched(1, 0, 1, 9),
            done(2, 100, 1, 9),
            cbops(3, 101, 1, &[400]),
            sched(4, 200, 1, 9),
            done(5, 1200, 1, 9),
            cbops(6, 1201, 1, &[400]),
        ];
        let scales = compute_op_time_scales(&events);
        assert_eq!(scales.clamped_cb_count(), 1);
        apply_op_time_scales(&mut events, &scales);
        assert_eq!(op_sums(&events), vec![vec![100], vec![400]]);
    }

    #[test]
    fn missing_scheduled_event_skips_the_clamp() {
        let mut events = vec![done(1, 100, 1, 9), cbops(2, 101, 1, &[400])];
        let scales = compute_op_time_scales(&events);
        assert_eq!(scales.clamped_cb_count(), 0);
        apply_op_time_scales(&mut events, &scales);
        assert_eq!(op_sums(&events), vec![vec![400]]);
    }

    #[test]
    fn scaled_gpu_ns_reads_without_mutation() {
        let events = vec![
            sched(1, 0, 1, 9),
            done(2, 100, 1, 9),
            cbops(3, 101, 1, &[300, 100]),
        ];
        let scales = compute_op_time_scales(&events);
        assert_eq!(scales.scaled_gpu_ns(3, 300), 75);
        assert_eq!(scales.scaled_gpu_ns(3, 100), 25);
        // Unknown seq: identity.
        assert_eq!(scales.scaled_gpu_ns(42, 300), 300);
    }
}

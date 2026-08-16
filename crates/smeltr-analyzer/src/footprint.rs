//! Per-PID aggregation of `ProcFootprint` samples.

use smeltr_core::event::{Event, Payload};
use std::collections::HashMap;

/// Aggregated footprint of one process in the traced tree, over the whole
/// session.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProcFootprintSummary {
    pub pid: u32,
    pub name: String,
    /// Largest footprint the probe observed.
    pub peak_bytes: u64,
    /// Largest `ri_lifetime_max_phys_footprint` the kernel reported — it can
    /// exceed `peak_bytes` when a spike fell between two samples.
    pub lifetime_max_bytes: u64,
    pub is_traced_root: bool,
    pub sample_count: usize,
}

/// Aggregates `ProcFootprint` samples by PID, sorted by descending peak.
pub fn compute_footprint_summary(events: &[Event]) -> Vec<ProcFootprintSummary> {
    let mut by_pid: HashMap<u32, ProcFootprintSummary> = HashMap::new();
    for e in events {
        let Payload::ProcFootprint {
            pid,
            name,
            phys_footprint_bytes,
            lifetime_max_phys_footprint_bytes,
            is_traced_root,
        } = &e.payload
        else {
            continue;
        };
        let entry = by_pid.entry(*pid).or_insert_with(|| ProcFootprintSummary {
            pid: *pid,
            name: name.clone(),
            peak_bytes: 0,
            lifetime_max_bytes: 0,
            is_traced_root: *is_traced_root,
            sample_count: 0,
        });
        entry.peak_bytes = entry.peak_bytes.max(*phys_footprint_bytes);
        entry.lifetime_max_bytes = entry
            .lifetime_max_bytes
            .max(*lifetime_max_phys_footprint_bytes);
        entry.sample_count += 1;
    }
    let mut out: Vec<ProcFootprintSummary> = by_pid.into_values().collect();
    out.sort_by(|a, b| b.peak_bytes.cmp(&a.peak_bytes).then(a.pid.cmp(&b.pid)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Payload, Source};
    use uuid::Uuid;

    fn ev(ts: u64, pid: u32, name: &str, bytes: u64, root: bool) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source: Source::Proc,
            pid: None,
            seq: ts,
            payload: Payload::ProcFootprint {
                pid,
                name: name.into(),
                phys_footprint_bytes: bytes,
                lifetime_max_phys_footprint_bytes: bytes + 1,
                is_traced_root: root,
            },
        }
    }

    #[test]
    fn summary_keeps_peak_per_pid() {
        let events = vec![
            ev(1, 100, "python", 5_000, true),
            ev(2, 100, "python", 9_000, true),
            ev(3, 100, "python", 7_000, true),
        ];
        let s = compute_footprint_summary(&events);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].pid, 100);
        assert_eq!(s[0].peak_bytes, 9_000);
        assert_eq!(s[0].lifetime_max_bytes, 9_001);
        assert_eq!(s[0].sample_count, 3);
        assert!(s[0].is_traced_root);
    }

    #[test]
    fn summary_is_sorted_by_peak_descending() {
        let events = vec![
            ev(1, 100, "small", 1_000, true),
            ev(2, 200, "big", 9_000, false),
            ev(3, 300, "mid", 5_000, false),
        ];
        let s = compute_footprint_summary(&events);
        let pids: Vec<u32> = s.iter().map(|x| x.pid).collect();
        assert_eq!(pids, vec![200, 300, 100]);
    }

    #[test]
    fn no_footprint_events_yields_empty() {
        assert!(compute_footprint_summary(&[]).is_empty());
    }
}

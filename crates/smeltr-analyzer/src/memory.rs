//! Per-scope memory aggregation: peak/avg/end of MTLDevice samples and
//! peak heap state during each scope window.

use serde::{Deserialize, Serialize};
use smeltr_core::event::{Event, Payload};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ScopeMemory {
    pub qualname: String,
    pub peak_bytes: u64,
    pub avg_bytes: u64,
    pub end_bytes: u64,
    pub sample_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HeapMemory {
    pub qualname: String,
    pub peak_heap_count: u32,
    pub peak_heap_bytes: u64,
}

/// Compute per-scope device memory stats from `MetalDeviceMemSample`
/// events. Returns one `ScopeMemory` per qualname, with the max-peak
/// record kept across multiple call sites of the same qualname. Sorted
/// by `peak_bytes` desc.
pub fn compute_memory_breakdown(events: &[Event]) -> Vec<ScopeMemory> {
    #[derive(Default)]
    struct Accum {
        peak: u64,
        sum: u128,
        count: u64,
        last: u64,
    }
    let mut stack: Vec<(String, Accum)> = Vec::new();
    let mut by_qualname: HashMap<String, ScopeMemory> = HashMap::new();

    for ev in events {
        match &ev.payload {
            Payload::ModuleEntered { qualname, .. } => {
                stack.push((qualname.clone(), Accum::default()));
            }
            Payload::ModuleReturned { .. } => {
                if let Some((qualname, accum)) = stack.pop() {
                    let avg = if accum.count > 0 {
                        (accum.sum / accum.count as u128) as u64
                    } else {
                        0
                    };
                    let sm = ScopeMemory {
                        qualname: qualname.clone(),
                        peak_bytes: accum.peak,
                        avg_bytes: avg,
                        end_bytes: accum.last,
                        sample_count: accum.count,
                    };
                    by_qualname
                        .entry(qualname)
                        .and_modify(|existing| {
                            if sm.peak_bytes > existing.peak_bytes {
                                *existing = sm.clone();
                            }
                        })
                        .or_insert(sm);
                }
            }
            Payload::MetalDeviceMemSample {
                allocated_bytes, ..
            } => {
                for (_, accum) in stack.iter_mut() {
                    if *allocated_bytes > accum.peak {
                        accum.peak = *allocated_bytes;
                    }
                    accum.sum += *allocated_bytes as u128;
                    accum.count += 1;
                    accum.last = *allocated_bytes;
                }
            }
            _ => {}
        }
    }

    let mut out: Vec<ScopeMemory> = by_qualname.into_values().collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.peak_bytes));
    out
}

/// Compute per-scope heap state peak. Walks `MetalHeapAlloc/Free` to
/// maintain `live_heaps`; on each mutation OR scope event, updates each
/// open scope's `peak_heap_count` / `peak_heap_bytes`. Returns one entry
/// per qualname (max across call sites). Sorted by `peak_heap_bytes`
/// desc.
pub fn compute_heap_breakdown(events: &[Event]) -> Vec<HeapMemory> {
    #[derive(Default)]
    struct Accum {
        peak_count: u32,
        peak_bytes: u64,
    }
    let mut stack: Vec<(String, Accum)> = Vec::new();
    let mut live_heaps: HashMap<u64, u64> = HashMap::new();
    let mut by_qualname: HashMap<String, HeapMemory> = HashMap::new();

    fn update_open_scopes(stack: &mut [(String, Accum)], live: &HashMap<u64, u64>) {
        let cur_count = live.len() as u32;
        let cur_bytes: u64 = live.values().sum();
        for (_, accum) in stack.iter_mut() {
            if cur_count > accum.peak_count {
                accum.peak_count = cur_count;
            }
            if cur_bytes > accum.peak_bytes {
                accum.peak_bytes = cur_bytes;
            }
        }
    }

    for ev in events {
        match &ev.payload {
            Payload::ModuleEntered { qualname, .. } => {
                stack.push((qualname.clone(), Accum::default()));
                update_open_scopes(&mut stack, &live_heaps);
            }
            Payload::ModuleReturned { .. } => {
                if let Some((qualname, accum)) = stack.pop() {
                    let hm = HeapMemory {
                        qualname: qualname.clone(),
                        peak_heap_count: accum.peak_count,
                        peak_heap_bytes: accum.peak_bytes,
                    };
                    by_qualname
                        .entry(qualname)
                        .and_modify(|existing| {
                            if hm.peak_heap_bytes > existing.peak_heap_bytes {
                                *existing = hm.clone();
                            }
                        })
                        .or_insert(hm);
                }
            }
            Payload::MetalHeapAlloc {
                heap_id,
                size_bytes,
                ..
            } => {
                live_heaps.insert(*heap_id, *size_bytes);
                update_open_scopes(&mut stack, &live_heaps);
            }
            Payload::MetalHeapFree { heap_id } => {
                live_heaps.remove(heap_id);
                update_open_scopes(&mut stack, &live_heaps);
            }
            _ => {}
        }
    }

    let mut out: Vec<HeapMemory> = by_qualname.into_values().collect();
    out.sort_by_key(|h| std::cmp::Reverse(h.peak_heap_bytes));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::Source;
    use uuid::Uuid;

    fn ev(seq: u64, ts: u64, source: Source, payload: Payload) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source,
            pid: None,
            seq,
            payload,
        }
    }

    fn enter(seq: u64, ts: u64, qualname: &str) -> Event {
        ev(
            seq,
            ts,
            Source::PythonSidecar,
            Payload::ModuleEntered {
                module_call_id: seq,
                module_def_id: 0,
                qualname: qualname.into(),
                class_name: "Scope".into(),
                parent_call_id: None,
                depth: 0,
            },
        )
    }

    fn ret(seq: u64, ts: u64, mid: u64) -> Event {
        ev(
            seq,
            ts,
            Source::PythonSidecar,
            Payload::ModuleReturned {
                module_call_id: mid,
            },
        )
    }

    fn sample(seq: u64, ts: u64, allocated: u64) -> Event {
        ev(
            seq,
            ts,
            Source::MetalHook,
            Payload::MetalDeviceMemSample {
                allocated_bytes: allocated,
                recommended_max_bytes: 16_000_000_000,
                at_event: "cb_committed".into(),
            },
        )
    }

    fn heap_alloc(seq: u64, ts: u64, heap_id: u64, size: u64) -> Event {
        ev(
            seq,
            ts,
            Source::MetalHook,
            Payload::MetalHeapAlloc {
                heap_id,
                size_bytes: size,
                label: None,
            },
        )
    }

    fn heap_free(seq: u64, ts: u64, heap_id: u64) -> Event {
        ev(
            seq,
            ts,
            Source::MetalHook,
            Payload::MetalHeapFree { heap_id },
        )
    }

    #[test]
    fn memory_empty_session_yields_empty() {
        assert!(compute_memory_breakdown(&[]).is_empty());
        assert!(compute_heap_breakdown(&[]).is_empty());
    }

    #[test]
    fn memory_peak_avg_end_computed_correctly() {
        let evs = vec![
            enter(1, 1, "foo"),
            sample(2, 2, 100),
            sample(3, 3, 500),
            sample(4, 4, 200),
            ret(5, 5, 1),
        ];
        let out = compute_memory_breakdown(&evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].qualname, "foo");
        assert_eq!(out[0].peak_bytes, 500);
        assert_eq!(out[0].avg_bytes, (100 + 500 + 200) / 3);
        assert_eq!(out[0].end_bytes, 200);
        assert_eq!(out[0].sample_count, 3);
    }

    #[test]
    fn memory_aggregates_multiple_scope_calls_takes_max_peak() {
        let evs = vec![
            enter(1, 1, "foo"),
            sample(2, 2, 300),
            ret(3, 3, 1),
            enter(4, 4, "foo"),
            sample(5, 5, 800),
            ret(6, 6, 4),
        ];
        let out = compute_memory_breakdown(&evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].peak_bytes, 800);
    }

    #[test]
    fn memory_scope_without_samples_emits_zero() {
        let evs = vec![enter(1, 1, "empty"), ret(2, 2, 1)];
        let out = compute_memory_breakdown(&evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].peak_bytes, 0);
        assert_eq!(out[0].avg_bytes, 0);
        assert_eq!(out[0].end_bytes, 0);
        assert_eq!(out[0].sample_count, 0);
    }

    #[test]
    fn heap_peak_count_and_bytes() {
        let evs = vec![
            enter(1, 1, "scope"),
            heap_alloc(2, 2, 100, 1000),
            heap_alloc(3, 3, 200, 2000),
            heap_free(4, 4, 100),
            ret(5, 5, 1),
        ];
        let out = compute_heap_breakdown(&evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].peak_heap_count, 2);
        assert_eq!(out[0].peak_heap_bytes, 3000);
    }

    #[test]
    fn heap_orphan_free_is_ignored() {
        let evs = vec![
            enter(1, 1, "scope"),
            heap_free(2, 2, 99),
            heap_alloc(3, 3, 100, 500),
            ret(4, 4, 1),
        ];
        let out = compute_heap_breakdown(&evs);
        assert_eq!(out[0].peak_heap_count, 1);
        assert_eq!(out[0].peak_heap_bytes, 500);
    }
}

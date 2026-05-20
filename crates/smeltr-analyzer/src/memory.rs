//! Per-scope memory aggregation: peak/avg/end of MTLDevice samples and
//! peak heap state during each scope window.

use serde::{Deserialize, Serialize};
use smeltr_core::event::{Event, Payload};
use std::collections::HashMap;

/// Async grace window: Metal CB-committed/completed events arrive up to
/// ~500 ms after the Python scope that triggered them has already returned.
const ASYNC_GRACE_NS: u64 = 500_000_000;

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
///
/// Fix for #40: scopes that have already returned are kept in a `draining`
/// list for up to `ASYNC_GRACE_NS` (500 ms) so that async Metal CB
/// committed/completed samples still credit the scope that launched the
/// work.
pub fn compute_memory_breakdown(events: &[Event]) -> Vec<ScopeMemory> {
    #[derive(Default)]
    struct Accum {
        peak: u64,
        sum: u128,
        count: u64,
        last: u64,
    }
    struct OpenScope {
        qualname: String,
        accum: Accum,
    }
    struct DrainingScope {
        qualname: String,
        accum: Accum,
        deadline_ns: u64,
    }

    let mut stack: Vec<OpenScope> = Vec::new();
    let mut draining: Vec<DrainingScope> = Vec::new();
    let mut by_qualname: HashMap<String, ScopeMemory> = HashMap::new();

    let finalize = |qualname: String, accum: Accum, map: &mut HashMap<String, ScopeMemory>| {
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
        map.entry(qualname)
            .and_modify(|existing| {
                if sm.peak_bytes > existing.peak_bytes {
                    *existing = sm.clone();
                }
            })
            .or_insert(sm);
    };

    for ev in events {
        // Sweep expired draining scopes BEFORE handling this event.
        let mut i = 0;
        while i < draining.len() {
            if draining[i].deadline_ns < ev.ts_mono_ns {
                let d = draining.swap_remove(i);
                finalize(d.qualname, d.accum, &mut by_qualname);
            } else {
                i += 1;
            }
        }

        match &ev.payload {
            Payload::ModuleEntered { qualname, .. } => {
                stack.push(OpenScope {
                    qualname: qualname.clone(),
                    accum: Accum::default(),
                });
            }
            Payload::ModuleReturned { .. } => {
                if let Some(open) = stack.pop() {
                    draining.push(DrainingScope {
                        qualname: open.qualname,
                        accum: open.accum,
                        deadline_ns: ev.ts_mono_ns.saturating_add(ASYNC_GRACE_NS),
                    });
                }
            }
            Payload::MetalDeviceMemSample {
                allocated_bytes, ..
            } => {
                for OpenScope { accum, .. } in stack.iter_mut() {
                    if *allocated_bytes > accum.peak {
                        accum.peak = *allocated_bytes;
                    }
                    accum.sum += *allocated_bytes as u128;
                    accum.count += 1;
                    accum.last = *allocated_bytes;
                }
                for DrainingScope {
                    accum, deadline_ns, ..
                } in draining.iter_mut()
                {
                    if ev.ts_mono_ns <= *deadline_ns {
                        if *allocated_bytes > accum.peak {
                            accum.peak = *allocated_bytes;
                        }
                        accum.sum += *allocated_bytes as u128;
                        accum.count += 1;
                        accum.last = *allocated_bytes;
                    }
                }
            }
            _ => {}
        }
    }

    // Flush any remaining draining scopes (deadlines irrelevant at end of stream).
    for d in draining.drain(..) {
        finalize(d.qualname, d.accum, &mut by_qualname);
    }
    // Flush any still-open scopes (no matching ModuleReturned in the stream).
    for o in stack.drain(..) {
        finalize(o.qualname, o.accum, &mut by_qualname);
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
///
/// Fix for #40: same async-grace window applied to draining scopes.
pub fn compute_heap_breakdown(events: &[Event]) -> Vec<HeapMemory> {
    #[derive(Default)]
    struct HeapAccum {
        peak_count: u32,
        peak_bytes: u64,
    }
    struct OpenHeapScope {
        qualname: String,
        accum: HeapAccum,
    }
    struct DrainingHeapScope {
        qualname: String,
        accum: HeapAccum,
        deadline_ns: u64,
    }

    fn update_open_scopes_heap(
        stack: &mut [OpenHeapScope],
        draining: &mut [DrainingHeapScope],
        live: &HashMap<u64, u64>,
        now_ns: u64,
    ) {
        let cur_count = live.len() as u32;
        let cur_bytes: u64 = live.values().sum();
        for OpenHeapScope { accum, .. } in stack.iter_mut() {
            if cur_count > accum.peak_count {
                accum.peak_count = cur_count;
            }
            if cur_bytes > accum.peak_bytes {
                accum.peak_bytes = cur_bytes;
            }
        }
        for DrainingHeapScope {
            accum, deadline_ns, ..
        } in draining.iter_mut()
        {
            if now_ns <= *deadline_ns {
                if cur_count > accum.peak_count {
                    accum.peak_count = cur_count;
                }
                if cur_bytes > accum.peak_bytes {
                    accum.peak_bytes = cur_bytes;
                }
            }
        }
    }

    let mut stack: Vec<OpenHeapScope> = Vec::new();
    let mut draining: Vec<DrainingHeapScope> = Vec::new();
    let mut live_heaps: HashMap<u64, u64> = HashMap::new();
    let mut by_qualname: HashMap<String, HeapMemory> = HashMap::new();

    let finalize_heap =
        |qualname: String, accum: HeapAccum, map: &mut HashMap<String, HeapMemory>| {
            let hm = HeapMemory {
                qualname: qualname.clone(),
                peak_heap_count: accum.peak_count,
                peak_heap_bytes: accum.peak_bytes,
            };
            map.entry(qualname)
                .and_modify(|existing| {
                    if hm.peak_heap_bytes > existing.peak_heap_bytes {
                        *existing = hm.clone();
                    }
                })
                .or_insert(hm);
        };

    for ev in events {
        // Sweep expired draining scopes BEFORE handling this event.
        let mut i = 0;
        while i < draining.len() {
            if draining[i].deadline_ns < ev.ts_mono_ns {
                let d = draining.swap_remove(i);
                finalize_heap(d.qualname, d.accum, &mut by_qualname);
            } else {
                i += 1;
            }
        }

        match &ev.payload {
            Payload::ModuleEntered { qualname, .. } => {
                stack.push(OpenHeapScope {
                    qualname: qualname.clone(),
                    accum: HeapAccum::default(),
                });
                update_open_scopes_heap(&mut stack, &mut draining, &live_heaps, ev.ts_mono_ns);
            }
            Payload::ModuleReturned { .. } => {
                if let Some(open) = stack.pop() {
                    draining.push(DrainingHeapScope {
                        qualname: open.qualname,
                        accum: open.accum,
                        deadline_ns: ev.ts_mono_ns.saturating_add(ASYNC_GRACE_NS),
                    });
                }
            }
            Payload::MetalHeapAlloc {
                heap_id,
                size_bytes,
                ..
            } => {
                live_heaps.insert(*heap_id, *size_bytes);
                update_open_scopes_heap(&mut stack, &mut draining, &live_heaps, ev.ts_mono_ns);
            }
            Payload::MetalHeapFree { heap_id } => {
                live_heaps.remove(heap_id);
                update_open_scopes_heap(&mut stack, &mut draining, &live_heaps, ev.ts_mono_ns);
            }
            _ => {}
        }
    }

    // Flush remaining draining scopes.
    for d in draining.drain(..) {
        finalize_heap(d.qualname, d.accum, &mut by_qualname);
    }
    // Flush still-open scopes.
    for o in stack.drain(..) {
        finalize_heap(o.qualname, o.accum, &mut by_qualname);
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
                fields: Default::default(),
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

    // ── pre-existing tests ────────────────────────────────────────────────

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

    // ── new tests: async-grace for compute_memory_breakdown (#40) ─────────

    #[test]
    fn compute_memory_breakdown_attributes_sample_after_scope_return() {
        // Repro for issue #40: scope exits at t=15, MetalDeviceMemSample lands
        // at t=100 (85 ns post-return, inside the 500 ms grace window).
        let evs = vec![
            enter(1, 10, "foo"),
            ret(2, 15, 1),
            sample(3, 100, 100 * 1024 * 1024),
        ];
        let out = compute_memory_breakdown(&evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].qualname, "foo");
        assert_eq!(out[0].peak_bytes, 100 * 1024 * 1024);
        assert_eq!(out[0].sample_count, 1);
    }

    #[test]
    fn compute_memory_breakdown_drops_sample_past_grace() {
        // 600 ms past return — outside the 500 ms grace.
        let evs = vec![
            enter(1, 10, "foo"),
            ret(2, 15, 1),
            sample(3, 15 + 600_000_000, 100 * 1024 * 1024),
        ];
        let out = compute_memory_breakdown(&evs);
        assert_eq!(out.len(), 1, "scope still finalizes (with 0 samples)");
        assert_eq!(out[0].qualname, "foo");
        assert_eq!(out[0].sample_count, 0);
    }

    #[test]
    fn compute_memory_breakdown_nested_scopes_both_get_drain_sample() {
        let evs = vec![
            enter(1, 5, "outer"),
            enter(2, 10, "inner"),
            ret(3, 15, 2),
            ret(4, 20, 1),
            sample(5, 100, 50 * 1024 * 1024),
        ];
        let out = compute_memory_breakdown(&evs);
        assert_eq!(out.len(), 2);
        for sm in &out {
            assert_eq!(sm.sample_count, 1, "{}: expected 1 sample", sm.qualname);
            assert_eq!(sm.peak_bytes, 50 * 1024 * 1024);
        }
    }

    #[test]
    fn compute_memory_breakdown_sequential_scopes_grace_overlap() {
        // Scope A returns at t=15. Scope B opens at t=20. Sample at t=100
        // falls within A's grace AND B's stack-window → both should record it.
        let evs = vec![
            enter(1, 10, "A"),
            ret(2, 15, 1),
            enter(3, 20, "B"),
            ret(4, 30, 3),
            sample(5, 100, 1024),
        ];
        let out = compute_memory_breakdown(&evs);
        assert_eq!(out.len(), 2);
        for sm in &out {
            assert_eq!(sm.sample_count, 1, "{}: expected 1 sample", sm.qualname);
        }
    }

    // ── new tests: async-grace for compute_heap_breakdown (#40) ──────────

    #[test]
    fn compute_heap_breakdown_attributes_alloc_after_scope_return() {
        // Heap alloc at t=100 (85 ns post-return) — within the 500 ms grace.
        let evs = vec![
            enter(1, 10, "foo"),
            ret(2, 15, 1),
            heap_alloc(3, 100, 42, 2_000_000),
        ];
        let out = compute_heap_breakdown(&evs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].qualname, "foo");
        assert_eq!(out[0].peak_heap_count, 1);
        assert_eq!(out[0].peak_heap_bytes, 2_000_000);
    }

    #[test]
    fn compute_heap_breakdown_drops_alloc_past_grace() {
        // 600 ms past return — outside the 500 ms grace.
        let evs = vec![
            enter(1, 10, "foo"),
            ret(2, 15, 1),
            heap_alloc(3, 15 + 600_000_000, 42, 2_000_000),
        ];
        let out = compute_heap_breakdown(&evs);
        assert_eq!(out.len(), 1, "scope still finalizes (with 0 heap)");
        assert_eq!(out[0].qualname, "foo");
        assert_eq!(out[0].peak_heap_count, 0);
        assert_eq!(out[0].peak_heap_bytes, 0);
    }

    #[test]
    fn compute_heap_breakdown_sequential_scopes_grace_overlap() {
        // Scope A returns at t=15. Scope B opens at t=20. Heap alloc at t=100
        // falls within A's grace AND B's stack-window → both should record it.
        let evs = vec![
            enter(1, 10, "A"),
            ret(2, 15, 1),
            enter(3, 20, "B"),
            ret(4, 30, 3),
            heap_alloc(5, 100, 7, 512_000),
        ];
        let out = compute_heap_breakdown(&evs);
        assert_eq!(out.len(), 2);
        for hm in &out {
            assert_eq!(
                hm.peak_heap_count, 1,
                "{}: expected peak_heap_count=1",
                hm.qualname
            );
            assert_eq!(
                hm.peak_heap_bytes, 512_000,
                "{}: expected 512_000 bytes",
                hm.qualname
            );
        }
    }
}

//! Aggregated UI state. Pure data; no rendering.

use smeltr_core::event::{Event, Payload, ProcEntry};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, Default)]
pub struct UiState {
    pub events_total: u64,
    pub timeline_buckets: VecDeque<(u64, u32)>,
    pub metal_queues: HashMap<u64, MetalQueueState>,
    pub mlx_memory: Option<MlxMemorySample>,
    pub mlx_eval_depth: u32,
    pub mlx_recent_marks: VecDeque<(u64, String)>,
    pub mlx_streams_seen: HashSet<String>,
    pub vm_sample: Option<VmSample>,
    pub proc_top: Vec<ProcEntry>,
    pub log_feed: VecDeque<LogEntry>,
    pub session_short: Option<String>,
    pub last_ts_wall_ns: u64,
    pub last_ts_mono_ns: u64,
}

#[derive(Debug, Clone, Default)]
pub struct MetalQueueState {
    pub depth: u32,
    pub in_flight: HashMap<u64, u64>,
    pub oldest_in_flight_cb: Option<(u64, u64)>,
    pub last_completed_status: Option<u32>,
    pub last_completed_error: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct MlxMemorySample {
    pub active_bytes: u64,
    pub peak_bytes: u64,
    pub cache_bytes: u64,
    pub ts_mono_ns: u64,
}

#[derive(Debug, Clone)]
pub struct VmSample {
    pub wired_bytes: u64,
    pub active_bytes: u64,
    pub compressed_bytes: u64,
    pub swap_used_bytes: u64,
    pub page_outs_per_sec: f32,
    pub ts_mono_ns: u64,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub ts_mono_ns: u64,
    pub kind: String,
    pub summary: String,
}

const TIMELINE_WINDOW_SEC: u64 = 60;
const RECENT_MARKS: usize = 10;
const LOG_FEED_CAP: usize = 200;

impl UiState {
    pub fn ingest(&mut self, ev: &Event) {
        self.events_total += 1;
        self.last_ts_wall_ns = ev.ts_wall_ns;
        self.last_ts_mono_ns = ev.ts_mono_ns;
        if self.session_short.is_none() {
            let s = ev.session_id.as_simple().to_string();
            self.session_short = Some(s[..s.len().min(8)].to_string());
        }
        self.bump_timeline(ev.ts_wall_ns / 1_000_000_000);
        self.ingest_payload(ev);
    }

    fn bump_timeline(&mut self, ts_sec: u64) {
        if let Some(last) = self.timeline_buckets.back_mut() {
            if last.0 == ts_sec {
                last.1 += 1;
                return;
            }
        }
        self.timeline_buckets.push_back((ts_sec, 1));
        let cutoff = ts_sec.saturating_sub(TIMELINE_WINDOW_SEC);
        while let Some(front) = self.timeline_buckets.front() {
            if front.0 < cutoff {
                self.timeline_buckets.pop_front();
            } else {
                break;
            }
        }
    }

    fn ingest_payload(&mut self, ev: &Event) {
        match &ev.payload {
            Payload::MetalCbCommitted {
                cb_id,
                queue_id,
                queue_depth,
                ..
            } => {
                let q = self.metal_queues.entry(*queue_id).or_default();
                q.depth = *queue_depth;
                q.in_flight.insert(*cb_id, ev.ts_mono_ns);
                q.recompute_oldest();
            }
            Payload::MetalCbScheduled { .. } => {}
            Payload::MetalCbCompleted {
                cb_id,
                queue_id,
                status,
                error_code,
                ..
            } => {
                let q = self.metal_queues.entry(*queue_id).or_default();
                q.in_flight.remove(cb_id);
                q.recompute_oldest();
                q.last_completed_status = Some(*status);
                q.last_completed_error = *error_code;
            }
            Payload::MlxMemoryPoll {
                active_bytes,
                peak_bytes,
                cache_bytes,
            } => {
                self.mlx_memory = Some(MlxMemorySample {
                    active_bytes: *active_bytes,
                    peak_bytes: *peak_bytes,
                    cache_bytes: *cache_bytes,
                    ts_mono_ns: ev.ts_mono_ns,
                });
            }
            Payload::MlxEvalEntered { stream, .. } => {
                self.mlx_eval_depth = self.mlx_eval_depth.saturating_add(1);
                self.mlx_streams_seen.insert(stream.clone());
            }
            Payload::MlxEvalReturned { .. } => {
                self.mlx_eval_depth = self.mlx_eval_depth.saturating_sub(1);
            }
            Payload::Mark { label } => {
                self.mlx_recent_marks
                    .push_back((ev.ts_mono_ns, label.clone()));
                while self.mlx_recent_marks.len() > RECENT_MARKS {
                    self.mlx_recent_marks.pop_front();
                }
            }
            Payload::VmSample {
                wired_bytes,
                active_bytes,
                compressed_bytes,
                swap_used_bytes,
                page_outs_per_sec,
            } => {
                self.vm_sample = Some(VmSample {
                    wired_bytes: *wired_bytes,
                    active_bytes: *active_bytes,
                    compressed_bytes: *compressed_bytes,
                    swap_used_bytes: *swap_used_bytes,
                    page_outs_per_sec: *page_outs_per_sec,
                    ts_mono_ns: ev.ts_mono_ns,
                });
            }
            Payload::ProcTop { top, .. } => {
                self.proc_top = top.clone();
            }
            Payload::OsLogLine {
                subsystem, message, ..
            } => {
                self.push_log(
                    ev.ts_mono_ns,
                    "oslog".into(),
                    format!("{}: {}", subsystem, truncate(message, 120)),
                );
            }
            Payload::CrashReportEmitted { path, summary, .. } => {
                self.push_log(
                    ev.ts_mono_ns,
                    "crash-rpt".into(),
                    format!("{} {}", basename(path), truncate(summary, 80)),
                );
            }
            Payload::MachException {
                target_pid,
                exception_type,
                ..
            } => {
                self.push_log(
                    ev.ts_mono_ns,
                    "mach-exc".into(),
                    format!("pid={target_pid} type={exception_type}"),
                );
            }
            Payload::PostMortemFlushed {
                reason,
                event_count,
                ..
            } => {
                self.push_log(
                    ev.ts_mono_ns,
                    "post-mortem".into(),
                    format!("{reason} ({event_count} events)"),
                );
            }
            Payload::MetalHookSkipped { reason } => {
                self.push_log(
                    ev.ts_mono_ns,
                    "metal-hook".into(),
                    format!("skipped: {reason}"),
                );
            }
            _ => {}
        }
    }

    fn push_log(&mut self, ts_mono_ns: u64, kind: String, summary: String) {
        self.log_feed.push_back(LogEntry {
            ts_mono_ns,
            kind,
            summary,
        });
        while self.log_feed.len() > LOG_FEED_CAP {
            self.log_feed.pop_front();
        }
    }
}

impl MetalQueueState {
    fn recompute_oldest(&mut self) {
        self.oldest_in_flight_cb = self
            .in_flight
            .iter()
            .min_by_key(|(_, ts)| *ts)
            .map(|(cb, ts)| (*cb, *ts));
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::Source;
    use uuid::Uuid;

    fn ev(ts: u64, payload: Payload) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source: Source::Mark,
            pid: None,
            seq: ts,
            payload,
        }
    }

    #[test]
    fn ingest_counts_events() {
        let mut s = UiState::default();
        s.ingest(&ev(0, Payload::Mark { label: "a".into() }));
        s.ingest(&ev(1_000_000_000, Payload::Mark { label: "b".into() }));
        assert_eq!(s.events_total, 2);
        assert_eq!(s.mlx_recent_marks.len(), 2);
    }

    #[test]
    fn metal_cb_lifecycle_tracks_in_flight() {
        let mut s = UiState::default();
        s.ingest(&ev(
            1,
            Payload::MetalCbCommitted {
                cb_id: 100,
                queue_id: 1,
                queue_depth: 5,
                label: None,
            },
        ));
        let q = s.metal_queues.get(&1).unwrap();
        assert_eq!(q.depth, 5);
        assert_eq!(q.in_flight.len(), 1);
        assert!(q.oldest_in_flight_cb.is_some());

        s.ingest(&ev(
            2,
            Payload::MetalCbCompleted {
                cb_id: 100,
                queue_id: 1,
                status: 4,
                error_code: None,
                error_domain: None,
                in_flight_ns: 1,
            },
        ));
        let q = s.metal_queues.get(&1).unwrap();
        assert_eq!(q.in_flight.len(), 0);
        assert!(q.oldest_in_flight_cb.is_none());
    }

    #[test]
    fn mlx_eval_depth_tracks_pairs() {
        let mut s = UiState::default();
        s.ingest(&ev(
            1,
            Payload::MlxEvalEntered {
                call_id: 1,
                array_count: 2,
                stream: "gpu".into(),
            },
        ));
        s.ingest(&ev(
            2,
            Payload::MlxEvalEntered {
                call_id: 2,
                array_count: 1,
                stream: "gpu".into(),
            },
        ));
        assert_eq!(s.mlx_eval_depth, 2);
        s.ingest(&ev(
            3,
            Payload::MlxEvalReturned {
                call_id: 1,
                duration_ns: 100,
                was_async: true,
            },
        ));
        assert_eq!(s.mlx_eval_depth, 1);
    }

    #[test]
    fn timeline_drops_old_buckets() {
        let mut s = UiState::default();
        for i in 0..120 {
            s.ingest(&ev(i * 1_000_000_000, Payload::Mark { label: "x".into() }));
        }
        assert!(s.timeline_buckets.len() <= 61);
        assert!(s.timeline_buckets.back().unwrap().0 == 119);
    }

    #[test]
    fn log_feed_ring_bounded() {
        let mut s = UiState::default();
        for i in 0..300 {
            s.ingest(&ev(
                i,
                Payload::OsLogLine {
                    ts_wall_ns: i,
                    subsystem: "x".into(),
                    category: "y".into(),
                    message: format!("m-{i}"),
                },
            ));
        }
        assert_eq!(s.log_feed.len(), 200);
    }
}

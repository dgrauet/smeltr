//! Memory footprint probe over the traced process tree.
//!
//! Unlike [`crate::probe::ProcProbe`], which samples the system-wide top 50 by
//! forking `/usr/bin/top`, this one targets only the traced process and its
//! descendants, and uses nothing but syscalls.

use crate::footprint::{descendants_of, list_processes, read_footprint, Footprint};
use async_trait::async_trait;
use smeltr_core::event::{Payload, Source};
use smeltr_probes_core::sink::SharedSink;
use smeltr_probes_core::{Probe, ProbeError, ProbeHealth};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Default cadence. Deliberately faster than `ProcProbe`'s (5s): a tick here
/// costs ~0.6ms of syscalls against 0.6s of forking and exec'ing `top` — a
/// thousand times less. Nothing forces this probe to slow down, and its
/// resolution is what yields the memory growth slope preceding a jetsam kill.
const DEFAULT_PERIOD: Duration = Duration::from_secs(2);

/// A zero `phys_bytes` means the process died (or turned zombie) between the
/// tree enumeration and the footprint read: `proc_pid_rusage` can succeed on a
/// dying process while reporting a null footprint. Emitting that zero would put
/// a false measurement in the session — indistinguishable from a genuine drop
/// to zero — skewing every later analysis (peak, growth slope, jetsam
/// correlation). We would rather omit it.
fn is_meaningful(f: &Footprint) -> bool {
    f.phys_bytes > 0
}

/// Samples the tree rooted at `root_pid`. Processes that vanish between the
/// enumeration and the read (or whose footprint reads as zero, see
/// [`is_meaningful`]) are simply omitted.
pub fn sample_tree(root_pid: u32) -> Vec<Payload> {
    let all = list_processes();
    descendants_of(root_pid, &all)
        .into_iter()
        .filter_map(|node| {
            let f = read_footprint(node.pid)?;
            if !is_meaningful(&f) {
                return None;
            }
            Some(Payload::ProcFootprint {
                pid: node.pid,
                name: node.name,
                phys_footprint_bytes: f.phys_bytes,
                lifetime_max_phys_footprint_bytes: f.lifetime_max_bytes,
                is_traced_root: node.pid == root_pid,
            })
        })
        .collect()
}

pub struct FootprintProbe {
    pid: u32,
    period: Duration,
}

impl FootprintProbe {
    pub fn new(pid: u32, period: Duration) -> Self {
        Self { pid, period }
    }

    /// Default cadence, overridable through `SMELTR_FOOTPRINT_PERIOD_MS`.
    /// An unparseable or zero value falls back to the default.
    pub fn default_period() -> Duration {
        std::env::var("SMELTR_FOOTPRINT_PERIOD_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_PERIOD)
    }
}

#[async_trait]
impl Probe for FootprintProbe {
    fn name(&self) -> &'static str {
        "footprint"
    }

    fn health(&self) -> ProbeHealth {
        ProbeHealth::Ok
    }

    async fn run(&mut self, sink: SharedSink, cancel: CancellationToken) -> Result<(), ProbeError> {
        if !cfg!(target_os = "macos") {
            return Err(ProbeError::Unavailable(
                "footprint probe requires macOS".into(),
            ));
        }
        let mut interval = tokio::time::interval(self.period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = interval.tick() => {}
            }
            // An empty tree means the traced process is gone: keep ticking,
            // detaching the probe is the daemon's job.
            for payload in sample_tree(self.pid) {
                sink.emit(Source::Proc, Some(self.pid), payload);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::footprint::{list_processes, read_footprint};
    use smeltr_core::event::Payload;

    #[test]
    fn default_period_is_two_seconds() {
        assert_eq!(FootprintProbe::default_period(), Duration::from_secs(2));
    }

    #[test]
    #[serial_test::serial]
    fn env_var_overrides_period() {
        std::env::set_var("SMELTR_FOOTPRINT_PERIOD_MS", "500");
        assert_eq!(FootprintProbe::default_period(), Duration::from_millis(500));
        std::env::remove_var("SMELTR_FOOTPRINT_PERIOD_MS");
    }

    #[test]
    #[serial_test::serial]
    fn invalid_env_var_falls_back_to_default() {
        std::env::set_var("SMELTR_FOOTPRINT_PERIOD_MS", "pas-un-nombre");
        assert_eq!(FootprintProbe::default_period(), Duration::from_secs(2));
        std::env::remove_var("SMELTR_FOOTPRINT_PERIOD_MS");
    }

    #[test]
    fn sample_tree_marks_root_and_reads_self() {
        let me = std::process::id();
        let payloads = sample_tree(me);
        assert!(!payloads.is_empty(), "au moins le processus courant");
        let Payload::ProcFootprint {
            pid,
            is_traced_root,
            phys_footprint_bytes,
            ..
        } = &payloads[0]
        else {
            panic!("expected ProcFootprint, got {:?}", payloads[0]);
        };
        assert_eq!(*pid, me);
        assert!(*is_traced_root, "the root must be marked");
        assert!(*phys_footprint_bytes > 0);
    }

    #[test]
    fn sample_tree_of_dead_pid_is_empty() {
        assert!(sample_tree(0).is_empty());
    }

    #[test]
    fn is_meaningful_rejects_zero_footprint() {
        assert!(!is_meaningful(&Footprint {
            phys_bytes: 0,
            lifetime_max_bytes: 1234,
        }));
    }

    #[test]
    fn is_meaningful_accepts_nonzero_footprint() {
        assert!(is_meaningful(&Footprint {
            phys_bytes: 1,
            lifetime_max_bytes: 1,
        }));
    }

    /// Measures the cost of one full tick. Run with:
    ///   cargo test -p smeltr-probes-proc cost_of_one_tick -- --ignored --nocapture
    #[test]
    #[ignore = "cost measurement, not a correctness assertion"]
    fn cost_of_one_tick() {
        let me = std::process::id();
        // Warm up.
        let _ = sample_tree(me);

        let iters = 50;
        let t0 = std::time::Instant::now();
        let mut total = 0usize;
        for _ in 0..iters {
            total += sample_tree(me).len();
        }
        let per_tick = t0.elapsed() / iters;

        // Isolated cost of the enumeration, expected to dominate.
        let t1 = std::time::Instant::now();
        for _ in 0..iters {
            let _ = list_processes();
        }
        let per_list = t1.elapsed() / iters;

        // Isolated cost of a single footprint read.
        let t2 = std::time::Instant::now();
        for _ in 0..iters {
            let _ = read_footprint(me);
        }
        let per_read = t2.elapsed() / iters;

        println!("sample_tree      : {per_tick:?} / tick ({total} payloads sur {iters} ticks)");
        println!("list_processes   : {per_list:?} / appel");
        println!("read_footprint   : {per_read:?} / appel");
    }
}

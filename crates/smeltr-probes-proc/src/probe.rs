use crate::raw::{read_sys, top_and_flagged, DEFAULT_FLAG_CPU_PCT};
use async_trait::async_trait;
use smeltr_core::event::{Payload, ProcEntry, Source};
use smeltr_probes_core::sink::SharedSink;
use smeltr_probes_core::{Probe, ProbeError, ProbeHealth};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Cadence of the system-wide CPU sweep. Back to 2s now that a tick costs
/// 0.02s instead of 0.43s (#217): #220 had slowed it to 5s purely to stop
/// the probe burning a third of a core, and that reason is gone. The TUI
/// process panel consumes these samples live, so resolution is worth having.
const DEFAULT_PERIOD: Duration = Duration::from_secs(2);

pub struct ProcProbe {
    period: Duration,
    top_n: usize,
}

impl ProcProbe {
    pub fn new(period: Duration, top_n: usize) -> Self {
        Self { period, top_n }
    }

    /// Period from `SMELTR_PROC_PERIOD_MS`, falling back to [`DEFAULT_PERIOD`].
    ///
    /// Read in the daemon's process, like `SMELTR_FOOTPRINT_PERIOD_MS`: set it
    /// on the daemon's environment, not on the `smeltr record` invocation.
    pub fn default_period() -> Duration {
        std::env::var("SMELTR_PROC_PERIOD_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_PERIOD)
    }
}

#[async_trait]
impl Probe for ProcProbe {
    fn name(&self) -> &'static str {
        "proc"
    }
    fn health(&self) -> ProbeHealth {
        ProbeHealth::Ok
    }
    async fn run(&mut self, sink: SharedSink, cancel: CancellationToken) -> Result<(), ProbeError> {
        let mut interval = tokio::time::interval(self.period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = interval.tick() => {}
            }
            let samples = match read_sys() {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                    return Err(ProbeError::Unavailable(e.to_string()))
                }
                Err(e) => return Err(ProbeError::Transient(e.to_string())),
            };
            let (top, flagged) = top_and_flagged(samples, self.top_n, DEFAULT_FLAG_CPU_PCT);
            let top: Vec<ProcEntry> = top
                .into_iter()
                .map(|s| ProcEntry {
                    pid: s.pid,
                    name: s.name,
                    cpu_pct: s.cpu_pct,
                })
                .collect();
            sink.emit(Source::Proc, None, Payload::ProcTop { top, flagged });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn default_period_is_two_seconds() {
        assert_eq!(ProcProbe::default_period(), Duration::from_secs(2));
    }

    #[test]
    #[serial_test::serial]
    fn env_var_overrides_period() {
        std::env::set_var("SMELTR_PROC_PERIOD_MS", "500");
        assert_eq!(ProcProbe::default_period(), Duration::from_millis(500));
        std::env::remove_var("SMELTR_PROC_PERIOD_MS");
    }

    #[test]
    #[serial_test::serial]
    fn zero_and_garbage_fall_back_to_default() {
        std::env::set_var("SMELTR_PROC_PERIOD_MS", "0");
        assert_eq!(ProcProbe::default_period(), DEFAULT_PERIOD);
        std::env::set_var("SMELTR_PROC_PERIOD_MS", "later");
        assert_eq!(ProcProbe::default_period(), DEFAULT_PERIOD);
        std::env::remove_var("SMELTR_PROC_PERIOD_MS");
    }
}

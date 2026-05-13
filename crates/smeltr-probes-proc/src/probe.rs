use crate::raw::{read_sys, top_and_flagged, DEFAULT_FLAG_CPU_PCT};
use async_trait::async_trait;
use smeltr_core::event::{Payload, ProcEntry, Source};
use smeltr_probes_core::sink::SharedSink;
use smeltr_probes_core::{Probe, ProbeError, ProbeHealth};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct ProcProbe {
    period: Duration,
    top_n: usize,
}

impl ProcProbe {
    pub fn new(period: Duration, top_n: usize) -> Self {
        Self { period, top_n }
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

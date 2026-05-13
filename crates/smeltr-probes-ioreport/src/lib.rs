use async_trait::async_trait;
use smeltr_core::event::{Payload, Source};
use smeltr_probes_core::sink::SharedSink;
use smeltr_probes_core::{Probe, ProbeError, ProbeHealth};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct IoReportProbe {
    period: Duration,
}

impl IoReportProbe {
    pub fn new(period: Duration) -> Self {
        Self { period }
    }
}

#[async_trait]
impl Probe for IoReportProbe {
    fn name(&self) -> &'static str {
        "ioreport"
    }
    fn health(&self) -> ProbeHealth {
        ProbeHealth::Degraded(
            "v1: user-space IOReport limited; precise GPU residency comes from metal-hook (Plan 3)"
                .into(),
        )
    }
    async fn run(&mut self, sink: SharedSink, cancel: CancellationToken) -> Result<(), ProbeError> {
        let mut interval = tokio::time::interval(self.period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = interval.tick() => {}
            }
            sink.emit(
                Source::IoReport,
                None,
                Payload::IoReportSample {
                    gpu_residency_pct: None,
                    ane_residency_pct: None,
                    cpu_residency_pct: None,
                    gpu_power_mw: None,
                    gpu_freq_mhz: None,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_probes_core::sink::test_util::CapturingSink;
    use std::sync::Arc;

    #[tokio::test]
    async fn ioreport_emits_at_least_one_sample() {
        let sink = Arc::new(CapturingSink::default());
        let token = CancellationToken::new();
        let token2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            token2.cancel();
        });
        let mut p = IoReportProbe::new(Duration::from_millis(50));
        let s: SharedSink = sink.clone();
        p.run(s, token).await.unwrap();
        assert!(!sink.events.lock().unwrap().is_empty());
    }
}

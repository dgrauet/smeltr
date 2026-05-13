use crate::raw::{compute_rate, read_sys, VmRaw};
use async_trait::async_trait;
use smeltr_core::event::{Payload, Source};
use smeltr_probes_core::sink::SharedSink;
use smeltr_probes_core::{Probe, ProbeError, ProbeHealth};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct VmProbe {
    period: Duration,
}

impl VmProbe {
    pub fn new(period: Duration) -> Self {
        Self { period }
    }
}

#[async_trait]
impl Probe for VmProbe {
    fn name(&self) -> &'static str {
        "vm"
    }
    fn health(&self) -> ProbeHealth {
        ProbeHealth::Ok
    }
    async fn run(&mut self, sink: SharedSink, cancel: CancellationToken) -> Result<(), ProbeError> {
        let mut prev: Option<(VmRaw, std::time::Instant)> = None;
        let mut interval = tokio::time::interval(self.period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = interval.tick() => {}
            }
            let now_inst = std::time::Instant::now();
            let raw = match read_sys() {
                Ok(r) => r,
                Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                    return Err(ProbeError::Unavailable(e.to_string()));
                }
                Err(e) => return Err(ProbeError::Transient(e.to_string())),
            };
            let rate = if let Some((prev_raw, prev_t)) = &prev {
                compute_rate(prev_raw, &raw, (now_inst - *prev_t).as_secs_f32())
            } else {
                0.0
            };
            prev = Some((raw, now_inst));
            sink.emit(
                Source::Vm,
                None,
                Payload::VmSample {
                    wired_bytes: raw.wired_bytes,
                    active_bytes: raw.active_bytes,
                    compressed_bytes: raw.compressed_bytes,
                    swap_used_bytes: raw.swap_used_bytes,
                    page_outs_per_sec: rate,
                },
            );
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use smeltr_probes_core::sink::test_util::CapturingSink;
    use std::sync::Arc;

    #[tokio::test]
    async fn vm_probe_emits_at_least_one_sample() {
        let sink = Arc::new(CapturingSink::default());
        let token = CancellationToken::new();
        let token2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            token2.cancel();
        });
        let mut probe = VmProbe::new(Duration::from_millis(100));
        let sink_dyn: SharedSink = sink.clone();
        probe.run(sink_dyn, token).await.unwrap();
        let n = sink.events.lock().unwrap().len();
        assert!(n >= 1, "expected at least 1 sample, got {n}");
    }
}

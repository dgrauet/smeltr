pub mod translate;

use async_trait::async_trait;
use smeltr_core::event::Source;
use smeltr_metal_ring::{open_for_read, RingReader};
use smeltr_probes_core::sink::SharedSink;
use smeltr_probes_core::{Probe, ProbeError, ProbeHealth};
use std::path::PathBuf;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct MetalHookProbe {
    ring_path: PathBuf,
    target_pid: u32,
}

impl MetalHookProbe {
    pub fn new(target_pid: u32, ring_path: PathBuf) -> Self {
        Self {
            target_pid,
            ring_path,
        }
    }
}

#[async_trait]
impl Probe for MetalHookProbe {
    fn name(&self) -> &'static str {
        "metal-hook"
    }
    fn health(&self) -> ProbeHealth {
        ProbeHealth::Ok
    }

    async fn run(&mut self, sink: SharedSink, cancel: CancellationToken) -> Result<(), ProbeError> {
        let path = self.ring_path.clone();
        let pid = self.target_pid;

        // Wait briefly for the ring file to materialize (child may need a moment).
        let mut waited = Duration::ZERO;
        let max_wait = Duration::from_secs(5);
        while !path.exists() {
            if cancel.is_cancelled() {
                return Ok(());
            }
            if waited >= max_wait {
                return Err(ProbeError::Unavailable(format!(
                    "ring path never appeared: {}",
                    path.display()
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            waited += Duration::from_millis(100);
        }

        let mut reader: RingReader =
            open_for_read(&path).map_err(|e| ProbeError::Transient(format!("open ring: {e}")))?;

        let mut interval = tokio::time::interval(Duration::from_millis(10)); // 100 Hz
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = interval.tick() => {}
            }
            loop {
                match reader.next() {
                    Ok(Some(ev)) => {
                        let payload = translate::frame_to_payload(ev.frame);
                        sink.emit(Source::MetalHook, Some(pid), payload);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!(error = %e, "ring decode error; draining halted this tick");
                        break;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_metal_ring::create_ring;
    use smeltr_probes_core::sink::test_util::CapturingSink;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[tokio::test]
    async fn probe_drains_ring_events() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ring.bin");
        {
            let mut w = create_ring(&path, 1 << 16).unwrap();
            w.write_cb_committed(1, 0x42, 0xa1, 3, Some("eval"))
                .unwrap();
            w.write_cb_scheduled(2, 0x42, 0xa1).unwrap();
            w.write_cb_completed(3, 0x42, 0xa1, 4, None, None, 1_000_000)
                .unwrap();
        }
        let sink: Arc<CapturingSink> = Arc::default();
        let mut probe = MetalHookProbe::new(1234, path);
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel2.cancel();
        });
        let sink_dyn: SharedSink = sink.clone();
        probe.run(sink_dyn, cancel).await.unwrap();

        let evs = sink.events.lock().unwrap();
        assert_eq!(evs.len(), 3, "got {} events", evs.len());
        assert!(evs
            .iter()
            .all(|(s, p, _)| matches!(s, Source::MetalHook) && *p == Some(1234)));
    }

    #[tokio::test]
    async fn probe_times_out_if_ring_never_appears() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("never-exists.ring");
        let sink: Arc<CapturingSink> = Arc::default();
        let mut probe = MetalHookProbe::new(99, path);
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            c2.cancel();
        });
        let sink_dyn: SharedSink = sink.clone();
        let result = probe.run(sink_dyn, cancel).await;
        assert!(result.is_ok(), "expected clean cancel, got {result:?}");
    }
}

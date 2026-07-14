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

        // Corrupt frames are skipped by the reader (it always makes
        // progress), so we keep draining; surface the corruption in-session
        // and log it, throttled — an unthrottled warn here once spammed the
        // daemon log at 100 Hz for hours and filled the disk (#113).
        let mut decode_errors: u64 = 0;
        let mut last_dropped: u64 = 0;

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
                        decode_errors += 1;
                        if decode_errors == 1 || decode_errors.is_multiple_of(1000) {
                            tracing::warn!(
                                error = %e,
                                count = decode_errors,
                                "ring decode error; frame skipped"
                            );
                            sink.emit(
                                Source::MetalHook,
                                Some(pid),
                                smeltr_core::event::Payload::MetalHookSkipped {
                                    reason: format!("ring decode error (#{decode_errors}): {e}"),
                                },
                            );
                        }
                    }
                }
            }
            // Frames dropped by the writer (ring full) never appear in the
            // stream; surface the header counter delta in-session.
            let dropped = reader.header_snapshot().dropped;
            if dropped > last_dropped {
                sink.emit(
                    Source::MetalHook,
                    Some(pid),
                    smeltr_core::event::Payload::MetalHookDropped {
                        count: dropped - last_dropped,
                    },
                );
                last_dropped = dropped;
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

    /// #113 regression: a corrupt frame in the middle of the ring must not
    /// stall the drain. The probe surfaces the corruption as an in-session
    /// MetalHookSkipped diagnostic and still delivers the surrounding events.
    #[tokio::test]
    async fn probe_survives_corrupt_frame_and_surfaces_it() {
        use smeltr_core::event::Payload;
        use std::io::{Seek, SeekFrom, Write};

        let dir = tempdir().unwrap();
        let path = dir.path().join("ring.bin");
        {
            let mut w = create_ring(&path, 1 << 16).unwrap();
            w.write_buffer_free(1, 0xaaaa).unwrap();
            w.write_buffer_free(2, 0xbbbb).unwrap();
        }
        // Corrupt the first frame's kind (torn concurrent write).
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        f.seek(SeekFrom::Start(
            (smeltr_metal_ring::wire::RING_HEADER_BYTES + 4) as u64,
        ))
        .unwrap();
        f.write_all(&0x6C69_6E00u32.to_le_bytes()).unwrap();
        f.sync_all().unwrap();
        drop(f);

        let sink: Arc<CapturingSink> = Arc::default();
        let mut probe = MetalHookProbe::new(1234, path);
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c2.cancel();
        });
        let sink_dyn: SharedSink = sink.clone();
        probe.run(sink_dyn, cancel).await.unwrap();

        let evs = sink.events.lock().unwrap();
        assert!(
            evs.iter()
                .any(|(_, _, p)| matches!(p, Payload::MetalBufferFree { buffer_id: 0xbbbb })),
            "the frame after the corrupt one must still be delivered"
        );
        assert!(
            evs.iter().any(|(_, _, p)| matches!(
                p,
                Payload::MetalHookSkipped { reason } if reason.contains("ring decode error")
            )),
            "corruption must be surfaced in-session, got {evs:?}"
        );
    }

    /// Frames dropped by the writer (ring full) are invisible in the frame
    /// stream — the probe must surface the header `dropped` counter as an
    /// in-session MetalHookDropped event.
    #[tokio::test]
    async fn probe_surfaces_writer_drops() {
        use smeltr_core::event::Payload;

        let dir = tempdir().unwrap();
        let path = dir.path().join("ring.bin");
        {
            let mut w = create_ring(&path, 128).unwrap(); // tiny: overflows fast
            for _ in 0..50 {
                let _ = w.write_buffer_free(1, 0xaaaa);
            }
        }
        let sink: Arc<CapturingSink> = Arc::default();
        let mut probe = MetalHookProbe::new(1234, path);
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c2.cancel();
        });
        let sink_dyn: SharedSink = sink.clone();
        probe.run(sink_dyn, cancel).await.unwrap();

        let evs = sink.events.lock().unwrap();
        assert!(
            evs.iter().any(|(_, _, p)| matches!(
                p,
                Payload::MetalHookDropped { count } if *count > 0
            )),
            "writer drops must be surfaced, got {evs:?}"
        );
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

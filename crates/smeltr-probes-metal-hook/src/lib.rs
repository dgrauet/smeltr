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

        let mut rings = vec![RingDrain::open(path.clone())?];

        let mut interval = tokio::time::interval(Duration::from_millis(10)); // 100 Hz
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        let mut tick: u64 = 0;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = interval.tick() => {}
            }
            // Siblings written by further hooked processes (#264) can
            // appear at any time; look for new ones every 250 ms.
            if tick.is_multiple_of(25) {
                for p in smeltr_metal_ring::ring_family(&path) {
                    if rings.iter().any(|r| r.path == p) {
                        continue;
                    }
                    match RingDrain::open(p) {
                        Ok(r) => rings.push(r),
                        // Retried at the next scan.
                        Err(e) => tracing::debug!(error = %e, "sibling ring not readable yet"),
                    }
                }
            }
            tick += 1;
            for ring in &mut rings {
                ring.drain(&sink, pid);
            }
        }
    }
}

/// One ring file being drained.
struct RingDrain {
    path: PathBuf,
    reader: RingReader,
    decode_errors: u64,
    last_dropped: u64,
}

impl RingDrain {
    fn open(path: PathBuf) -> Result<Self, ProbeError> {
        let reader =
            open_for_read(&path).map_err(|e| ProbeError::Transient(format!("open ring: {e}")))?;
        Ok(Self {
            path,
            reader,
            decode_errors: 0,
            last_dropped: 0,
        })
    }

    /// Emit every frame written since the last drain.
    ///
    /// Corrupt frames are skipped by the reader (it always makes progress),
    /// so we keep draining; surface the corruption in-session and log it,
    /// throttled — an unthrottled warn here once spammed the daemon log at
    /// 100 Hz for hours and filled the disk (#113).
    fn drain(&mut self, sink: &SharedSink, pid: u32) {
        loop {
            match self.reader.next() {
                Ok(Some(ev)) => {
                    let payload = translate::frame_to_payload(ev.frame);
                    // The hook's own stamp (mach_absolute_time in ns), not
                    // the drain time (#244).
                    sink.emit_at(Source::MetalHook, Some(pid), ev.ts_mono_ns, payload);
                }
                Ok(None) => break,
                Err(e) => {
                    self.decode_errors += 1;
                    let n = self.decode_errors;
                    if n == 1 || n.is_multiple_of(1000) {
                        tracing::warn!(
                            error = %e,
                            count = n,
                            ring = %self.path.display(),
                            "ring decode error; frame skipped"
                        );
                        sink.emit(
                            Source::MetalHook,
                            Some(pid),
                            smeltr_core::event::Payload::MetalHookSkipped {
                                reason: format!("ring decode error (#{n}): {e}"),
                            },
                        );
                    }
                }
            }
        }
        // Frames dropped by the writer (ring full) never appear in the
        // stream; surface the header counter delta in-session.
        let dropped = self.reader.header_snapshot().dropped;
        if dropped > self.last_dropped {
            sink.emit(
                Source::MetalHook,
                Some(pid),
                smeltr_core::event::Payload::MetalHookDropped {
                    count: dropped - self.last_dropped,
                },
            );
            self.last_dropped = dropped;
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

    /// #244: each event keeps the hook's own timestamp. Stamped on receipt,
    /// a whole 10 ms drain landed on one instant: CB windows collapsed to
    /// microseconds and the #146 clamp crushed op times to match.
    #[tokio::test]
    async fn probe_keeps_the_hooks_timestamps() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ring.bin");
        {
            let mut w = create_ring(&path, 1 << 16).unwrap();
            w.write_cb_committed(1_000, 0x42, 0xa1, 1, None).unwrap();
            w.write_cb_scheduled(2_000, 0x42, 0xa1).unwrap();
            w.write_cb_completed(9_000, 0x42, 0xa1, 4, None, None, 7_000)
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

        let stamps = sink.stamps.lock().unwrap();
        assert_eq!(*stamps, vec![Some(1_000), Some(2_000), Some(9_000)]);
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

    /// #264: every hooked process after the first writes its own
    /// `<ring>.<pid>`, possibly long after the recording started. The probe
    /// drains those too, under the recorded pid (the router sends events to
    /// the session by that pid).
    #[tokio::test]
    async fn probe_drains_sibling_rings_that_appear_later() {
        use smeltr_core::event::Payload;

        let dir = tempdir().unwrap();
        let path = dir.path().join("abc.ring");
        {
            let mut w = create_ring(&path, 1 << 16).unwrap();
            w.write_buffer_free(1, 0xaaaa).unwrap();
        }
        let sibling = dir.path().join("abc.ring.4321");
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let mut w = create_ring(&sibling, 1 << 16).unwrap();
            w.write_buffer_free(2, 0xbbbb).unwrap();
            w.write_buffer_free(3, 0xcccc).unwrap();
        });
        let sink: Arc<CapturingSink> = Arc::default();
        let mut probe = MetalHookProbe::new(1234, path);
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            c2.cancel();
        });
        let sink_dyn: SharedSink = sink.clone();
        probe.run(sink_dyn, cancel).await.unwrap();

        let evs = sink.events.lock().unwrap();
        let freed: Vec<u64> = evs
            .iter()
            .filter_map(|(_, _, p)| match p {
                Payload::MetalBufferFree { buffer_id } => Some(*buffer_id),
                _ => None,
            })
            .collect();
        assert_eq!(freed, vec![0xaaaa, 0xbbbb, 0xcccc]);
        assert!(evs.iter().all(|(_, pid, _)| *pid == Some(1234)));
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

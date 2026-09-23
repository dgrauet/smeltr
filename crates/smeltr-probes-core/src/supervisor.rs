use crate::probe::{Probe, ProbeError, ProbeHealth};
use crate::sink::SharedSink;
use smeltr_core::event::{Payload, ProbeHealthState, Source};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const MAX_RESTARTS: u32 = 5;
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// A run that lasted this long was healthy: its failure starts the restart
/// count afresh. Without it, rare transient failures (a `log stream`
/// closing once a week) accumulated until the probe was disabled for good.
const HEALTHY_RUN: Duration = Duration::from_secs(60);

pub struct SupervisorHandle {
    cancel: CancellationToken,
    join_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl SupervisorHandle {
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        for h in std::mem::take(&mut self.join_handles) {
            let _ = h.await;
        }
    }
}

/// Dropping a handle stops its probes. Replacing a per-pid handle used to
/// drop the old one and leave its probes running, orphaned (#244).
impl Drop for SupervisorHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub struct Supervisor {
    sink: SharedSink,
    probes: Vec<Box<dyn Probe>>,
}

impl Supervisor {
    pub fn new(sink: SharedSink) -> Self {
        Self {
            sink,
            probes: Vec::new(),
        }
    }

    pub fn add(&mut self, probe: Box<dyn Probe>) {
        self.probes.push(probe);
    }

    pub fn spawn(self) -> SupervisorHandle {
        let cancel = CancellationToken::new();
        let mut handles = Vec::new();
        for probe in self.probes {
            let sink = self.sink.clone();
            let token = cancel.clone();
            handles.push(tokio::spawn(run_with_restart(probe, sink, token)));
        }
        SupervisorHandle {
            cancel,
            join_handles: handles,
        }
    }
}

async fn run_with_restart(mut probe: Box<dyn Probe>, sink: SharedSink, cancel: CancellationToken) {
    let name = probe.name();
    let mut attempt: u32 = 0;
    let mut backoff = INITIAL_BACKOFF;

    loop {
        if cancel.is_cancelled() {
            break;
        }

        emit_health(&sink, name, probe.health());

        // Run in its own task so a panic is observed and reported instead
        // of silently ending supervision (#244).
        let started = tokio::time::Instant::now();
        let (run_sink, run_cancel) = (sink.clone(), cancel.clone());
        let run = tokio::spawn(async move {
            let result = probe.run(run_sink, run_cancel).await;
            (probe, result)
        });
        let result = match run.await {
            Ok((p, result)) => {
                probe = p;
                result
            }
            Err(e) => {
                let reason = if e.is_panic() {
                    let payload = e.into_panic();
                    let msg = payload
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_default();
                    format!("panicked: {msg}")
                } else {
                    "probe task cancelled".to_string()
                };
                tracing::error!(probe = name, reason = %reason, "probe task ended");
                sink.emit(
                    Source::System,
                    None,
                    Payload::ProbeHealth {
                        probe: name.into(),
                        state: ProbeHealthState::Failed,
                        reason: Some(reason),
                    },
                );
                break;
            }
        };

        if cancel.is_cancelled() {
            break;
        }
        if started.elapsed() >= HEALTHY_RUN {
            attempt = 0;
            backoff = INITIAL_BACKOFF;
        }

        match result {
            Ok(()) => {
                tracing::info!(probe = name, "probe exited cleanly");
                break;
            }
            Err(ProbeError::PermissionDenied(reason)) | Err(ProbeError::Unavailable(reason)) => {
                tracing::warn!(probe = name, reason = %reason, "probe disabled");
                sink.emit(
                    Source::System,
                    None,
                    Payload::ProbeHealth {
                        probe: name.into(),
                        state: ProbeHealthState::Failed,
                        reason: Some(reason),
                    },
                );
                break;
            }
            Err(e) => {
                attempt += 1;
                tracing::warn!(probe = name, attempt, error = %e, "probe failed, restarting");
                if attempt >= MAX_RESTARTS {
                    sink.emit(
                        Source::System,
                        None,
                        Payload::ProbeHealth {
                            probe: name.into(),
                            state: ProbeHealthState::Failed,
                            reason: Some(format!("max restarts exceeded: {e}")),
                        },
                    );
                    break;
                }
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel.cancelled() => break,
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

fn emit_health(sink: &SharedSink, name: &str, health: ProbeHealth) {
    let (state, reason) = match health {
        ProbeHealth::Ok => (ProbeHealthState::Ok, None),
        ProbeHealth::Degraded(r) => (ProbeHealthState::Degraded, Some(r)),
        ProbeHealth::Failed(r) => (ProbeHealthState::Failed, Some(r)),
    };
    sink.emit(
        Source::System,
        None,
        Payload::ProbeHealth {
            probe: name.into(),
            state,
            reason,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::test_util::CapturingSink;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    struct FlakeyProbe {
        fails_left: Arc<Mutex<u32>>,
    }

    #[async_trait]
    impl Probe for FlakeyProbe {
        fn name(&self) -> &'static str {
            "flakey"
        }
        fn health(&self) -> ProbeHealth {
            ProbeHealth::Ok
        }
        async fn run(
            &mut self,
            sink: SharedSink,
            _cancel: CancellationToken,
        ) -> Result<(), ProbeError> {
            let mut left = self.fails_left.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                drop(left);
                return Err(ProbeError::Transient("boom".into()));
            }
            sink.emit(
                Source::System,
                None,
                Payload::Mark {
                    label: "alive".into(),
                    fields: Default::default(),
                },
            );
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn supervisor_retries_then_succeeds() {
        let sink = Arc::new(CapturingSink::default());
        let sink_dyn: SharedSink = sink.clone();
        let mut sup = Supervisor::new(sink_dyn);
        sup.add(Box::new(FlakeyProbe {
            fails_left: Arc::new(Mutex::new(2)),
        }));
        let handle = sup.spawn();
        tokio::time::sleep(Duration::from_secs(5)).await;
        handle.shutdown().await;

        let events = sink.events.lock().unwrap();
        assert!(events
            .iter()
            .any(|(_, _, p)| matches!(p, Payload::Mark { label, .. } if label == "alive")));
    }

    /// Runs `healthy` of simulated time, then fails — forever.
    struct LongRunsThenFails {
        runs: Arc<Mutex<u32>>,
        healthy: Duration,
    }

    #[async_trait]
    impl Probe for LongRunsThenFails {
        fn name(&self) -> &'static str {
            "long"
        }
        fn health(&self) -> ProbeHealth {
            ProbeHealth::Ok
        }
        async fn run(
            &mut self,
            _: SharedSink,
            cancel: CancellationToken,
        ) -> Result<(), ProbeError> {
            *self.runs.lock().unwrap() += 1;
            tokio::select! {
                _ = tokio::time::sleep(self.healthy) => {}
                _ = cancel.cancelled() => return Ok(()),
            }
            Err(ProbeError::Transient("log stream closed".into()))
        }
    }

    fn failed_health(sink: &CapturingSink) -> Vec<String> {
        sink.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(_, _, p)| match p {
                Payload::ProbeHealth {
                    state: ProbeHealthState::Failed,
                    reason,
                    ..
                } => Some(reason.clone().unwrap_or_default()),
                _ => None,
            })
            .collect()
    }

    /// #244: the failure count never reset, so five `log stream` closures
    /// spread over weeks disabled oslog for good. A probe that ran healthy
    /// for a long time starts its count afresh.
    #[tokio::test(start_paused = true)]
    async fn failures_far_apart_never_disable_a_probe() {
        let sink = Arc::new(CapturingSink::default());
        let runs = Arc::new(Mutex::new(0));
        let mut sup = Supervisor::new(sink.clone());
        sup.add(Box::new(LongRunsThenFails {
            runs: runs.clone(),
            healthy: Duration::from_secs(3600),
        }));
        let handle = sup.spawn();
        tokio::time::sleep(Duration::from_secs(10 * 3600)).await;
        handle.shutdown().await;
        assert!(*runs.lock().unwrap() >= 9, "runs: {}", runs.lock().unwrap());
        assert!(
            failed_health(&sink).is_empty(),
            "{:?}",
            failed_health(&sink)
        );
    }

    /// Rapid failures still give up after MAX_RESTARTS.
    #[tokio::test(start_paused = true)]
    async fn rapid_failures_still_disable_a_probe() {
        let sink = Arc::new(CapturingSink::default());
        let mut sup = Supervisor::new(sink.clone());
        sup.add(Box::new(LongRunsThenFails {
            runs: Arc::default(),
            healthy: Duration::from_millis(10),
        }));
        let handle = sup.spawn();
        tokio::time::sleep(Duration::from_secs(600)).await;
        handle.shutdown().await;
        let failed = failed_health(&sink);
        assert!(
            failed.iter().any(|r| r.contains("max restarts")),
            "{failed:?}"
        );
    }

    struct Panics;

    #[async_trait]
    impl Probe for Panics {
        fn name(&self) -> &'static str {
            "panics"
        }
        fn health(&self) -> ProbeHealth {
            ProbeHealth::Ok
        }
        async fn run(&mut self, _: SharedSink, _: CancellationToken) -> Result<(), ProbeError> {
            panic!("index out of bounds");
        }
    }

    /// #244: a panicking probe died silently — no health event, nothing in
    /// the session to say the probe was gone.
    #[tokio::test]
    async fn a_panicking_probe_is_reported_failed() {
        let sink = Arc::new(CapturingSink::default());
        let mut sup = Supervisor::new(sink.clone());
        sup.add(Box::new(Panics));
        let handle = sup.spawn();
        tokio::time::sleep(Duration::from_millis(100)).await;
        handle.shutdown().await;
        let failed = failed_health(&sink);
        assert!(failed.iter().any(|r| r.contains("panicked")), "{failed:?}");
    }

    struct UntilCancelled(Arc<std::sync::atomic::AtomicBool>);

    #[async_trait]
    impl Probe for UntilCancelled {
        fn name(&self) -> &'static str {
            "until-cancelled"
        }
        fn health(&self) -> ProbeHealth {
            ProbeHealth::Ok
        }
        async fn run(
            &mut self,
            _: SharedSink,
            cancel: CancellationToken,
        ) -> Result<(), ProbeError> {
            cancel.cancelled().await;
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// #244: replacing a handle (a pid recorded twice) dropped the old one,
    /// and dropping did not cancel — its probes ran on, orphaned.
    #[tokio::test]
    async fn dropping_the_handle_stops_its_probes() {
        let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut sup = Supervisor::new(Arc::new(CapturingSink::default()));
        sup.add(Box::new(UntilCancelled(stopped.clone())));
        let handle = sup.spawn();
        tokio::time::sleep(Duration::from_millis(50)).await; // probe running
        drop(handle);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(stopped.load(std::sync::atomic::Ordering::SeqCst));
    }
}

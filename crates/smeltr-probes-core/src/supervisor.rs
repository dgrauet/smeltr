use crate::probe::{Probe, ProbeError, ProbeHealth};
use crate::sink::SharedSink;
use smeltr_core::event::{Payload, ProbeHealthState, Source};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const MAX_RESTARTS: u32 = 5;
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

pub struct SupervisorHandle {
    cancel: CancellationToken,
    join_handles: Vec<tokio::task::JoinHandle<()>>,
}

impl SupervisorHandle {
    pub async fn shutdown(self) {
        self.cancel.cancel();
        for h in self.join_handles {
            let _ = h.await;
        }
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

        let result = probe.run(sink.clone(), cancel.clone()).await;

        if cancel.is_cancelled() {
            break;
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
}

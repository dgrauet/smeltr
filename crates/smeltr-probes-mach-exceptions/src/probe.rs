use crate::port::{install_for_pid, DecodedException};
use async_trait::async_trait;
use smeltr_core::event::{Payload, Source};
use smeltr_probes_core::sink::SharedSink;
use smeltr_probes_core::{Probe, ProbeError, ProbeHealth};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct MachExceptionsProbe {
    pub target_pid: u32,
}

impl MachExceptionsProbe {
    pub fn new(target_pid: u32) -> Self {
        Self { target_pid }
    }
}

#[async_trait]
impl Probe for MachExceptionsProbe {
    fn name(&self) -> &'static str {
        "mach-exceptions"
    }
    fn health(&self) -> ProbeHealth {
        ProbeHealth::Ok
    }

    async fn run(&mut self, sink: SharedSink, cancel: CancellationToken) -> Result<(), ProbeError> {
        let pid = self.target_pid;
        let receiver = install_for_pid(pid).map_err(|e| {
            if e.kind() == std::io::ErrorKind::Unsupported {
                ProbeError::Unavailable(e.to_string())
            } else {
                ProbeError::PermissionDenied(e.to_string())
            }
        })?;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DecodedException>();
        let token = cancel.clone();
        std::thread::spawn(move || loop {
            if token.is_cancelled() {
                break;
            }
            if let Some(mut exc) = receiver.next(Duration::from_millis(500)) {
                exc.target_pid = pid;
                if tx.send(exc).is_err() {
                    break;
                }
            }
        });

        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                msg = rx.recv() => {
                    let Some(exc) = msg else { return Ok(()); };
                    sink.emit(
                        Source::MachExc,
                        Some(pid),
                        Payload::MachException {
                            target_pid: exc.target_pid,
                            exception_type: exc.exception_type,
                            codes: exc.codes,
                        },
                    );
                }
            }
        }
    }
}

use crate::parse::{parse_line, predicate};
use async_trait::async_trait;
use smeltr_core::event::Source;
use smeltr_probes_core::sink::SharedSink;
use smeltr_probes_core::{Probe, ProbeError, ProbeHealth};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct OsLogProbe;

impl OsLogProbe {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Probe for OsLogProbe {
    fn name(&self) -> &'static str {
        "oslog"
    }
    fn health(&self) -> ProbeHealth {
        ProbeHealth::Ok
    }

    async fn run(&mut self, sink: SharedSink, cancel: CancellationToken) -> Result<(), ProbeError> {
        let mut child = Command::new("/usr/bin/log")
            .args(["stream", "--style", "ndjson", "--predicate", &predicate()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ProbeError::Unavailable(format!("spawn `log stream`: {e}")))?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProbeError::Transient("no stdout from log stream".into()))?;
        let mut lines = BufReader::new(stdout).lines();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = child.kill().await;
                    return Ok(());
                }
                line = lines.next_line() => {
                    let line = match line {
                        Ok(Some(l)) => l,
                        Ok(None) => return Err(ProbeError::Transient("log stream closed".into())),
                        Err(e) => return Err(ProbeError::Transient(e.to_string())),
                    };
                    if let Some(payload) = parse_line(&line) {
                        sink.emit(Source::OsLog, None, payload);
                    }
                }
            }
        }
    }
}

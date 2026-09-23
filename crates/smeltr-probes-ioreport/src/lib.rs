use async_trait::async_trait;
use smeltr_probes_core::sink::SharedSink;
use smeltr_probes_core::{Probe, ProbeError, ProbeHealth};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
pub struct IoReportProbe;

impl IoReportProbe {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Probe for IoReportProbe {
    fn name(&self) -> &'static str {
        "ioreport"
    }
    fn health(&self) -> ProbeHealth {
        ProbeHealth::Failed(NOT_IMPLEMENTED.into())
    }
    /// Not implemented: IOReport residency needs private frameworks, and
    /// precise GPU timing comes from the Metal hook. The stub used to write
    /// an all-`None` sample every second — 40 % of an ambient session's
    /// events, carrying nothing (#244). It now reports itself unavailable.
    async fn run(
        &mut self,
        _sink: SharedSink,
        _cancel: CancellationToken,
    ) -> Result<(), ProbeError> {
        Err(ProbeError::Unavailable(NOT_IMPLEMENTED.into()))
    }
}

const NOT_IMPLEMENTED: &str =
    "IOReport residency not implemented; GPU timing comes from the Metal hook";

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_probes_core::sink::test_util::CapturingSink;
    use std::sync::Arc;

    /// #244: no more empty samples — the probe says it is unavailable.
    #[tokio::test]
    async fn ioreport_reports_unavailable_and_emits_nothing() {
        let sink = Arc::new(CapturingSink::default());
        let mut p = IoReportProbe::new();
        let s: SharedSink = sink.clone();
        let r = p.run(s, CancellationToken::new()).await;
        assert!(matches!(r, Err(ProbeError::Unavailable(_))), "{r:?}");
        assert!(sink.events.lock().unwrap().is_empty());
    }
}

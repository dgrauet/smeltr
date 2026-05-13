use crate::sink::SharedSink;
use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeHealth {
    Ok,
    Degraded(String),
    Failed(String),
}

#[derive(Debug, Error)]
pub enum ProbeError {
    #[error("probe permission denied: {0}")]
    PermissionDenied(String),
    #[error("probe unavailable on this platform: {0}")]
    Unavailable(String),
    #[error("probe transient failure: {0}")]
    Transient(String),
    #[error("probe i/o: {0}")]
    Io(#[from] std::io::Error),
}

#[async_trait]
pub trait Probe: Send + 'static {
    fn name(&self) -> &'static str;
    fn health(&self) -> ProbeHealth;
    async fn run(
        &mut self,
        sink: SharedSink,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<(), ProbeError>;
}

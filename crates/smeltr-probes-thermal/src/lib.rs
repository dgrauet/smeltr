use async_trait::async_trait;
use smeltr_core::event::{Payload, Source};
use smeltr_probes_core::sink::SharedSink;
use smeltr_probes_core::{Probe, ProbeError, ProbeHealth};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct ThermalProbe {
    period: Duration,
}

impl ThermalProbe {
    pub fn new(period: Duration) -> Self {
        Self { period }
    }
}

pub fn read_state() -> std::io::Result<u32> {
    #[cfg(target_os = "macos")]
    unsafe {
        let mut val: i32 = 0;
        let mut size = std::mem::size_of::<i32>();
        let name = std::ffi::CString::new("kern.thermalstate").unwrap();
        let rc = libc::sysctlbyname(
            name.as_ptr(),
            &mut val as *mut _ as *mut _,
            &mut size,
            std::ptr::null_mut(),
            0,
        );
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(val.max(0) as u32)
    }
    #[cfg(not(target_os = "macos"))]
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "thermal probe requires macOS",
    ))
}

#[async_trait]
impl Probe for ThermalProbe {
    fn name(&self) -> &'static str {
        "thermal"
    }
    fn health(&self) -> ProbeHealth {
        ProbeHealth::Degraded("coarse: kern.thermalstate only (root for SMC)".into())
    }
    async fn run(&mut self, sink: SharedSink, cancel: CancellationToken) -> Result<(), ProbeError> {
        let mut interval = tokio::time::interval(self.period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last: Option<u32> = None;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = interval.tick() => {}
            }
            let level = match read_state() {
                Ok(v) => v,
                Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                    return Err(ProbeError::Unavailable(e.to_string()))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(ProbeError::Unavailable(format!(
                        "kern.thermalstate not available: {e}"
                    )))
                }
                Err(e) => return Err(ProbeError::Transient(e.to_string())),
            };
            if last != Some(level) {
                sink.emit(Source::Thermal, None, Payload::ThermalState { level });
                last = Some(level);
            }
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn read_state_does_not_error_on_macos() {
        // kern.thermalstate is only exposed on some macOS hardware (e.g. Intel
        // Macs). On Apple Silicon it may be absent — accept NotFound as a
        // graceful degradation path; otherwise the read should produce a
        // plausible level.
        match read_state() {
            Ok(v) => assert!(v <= 10, "implausible thermal level: {v}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => panic!("kern.thermalstate read failed: {e:?}"),
        }
    }
}

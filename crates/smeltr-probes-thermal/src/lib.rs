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

/// The system's thermal pressure level (`OSThermalPressureLevel`):
/// 0 nominal, 1 moderate, 2 heavy, 3 trapping, 4 sleeping.
///
/// Read from the `com.apple.system.thermalpressurelevel` notification state
/// (libSystem's notify API, unprivileged) — the value
/// `NSProcessInfo.thermalState` reflects. The `kern.thermalstate` sysctl
/// read before does not exist on Apple Silicon (#244).
pub fn read_state() -> std::io::Result<u32> {
    #[cfg(target_os = "macos")]
    {
        extern "C" {
            fn notify_register_check(name: *const std::ffi::c_char, token: *mut i32) -> u32;
            fn notify_get_state(token: i32, state: *mut u64) -> u32;
        }
        static TOKEN: std::sync::OnceLock<Option<i32>> = std::sync::OnceLock::new();
        let token = TOKEN.get_or_init(|| {
            let mut token = 0;
            let status = unsafe {
                notify_register_check(
                    c"com.apple.system.thermalpressurelevel".as_ptr(),
                    &mut token,
                )
            };
            (status == 0).then_some(token)
        });
        let Some(token) = *token else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "thermal pressure notification unavailable",
            ));
        };
        let mut state: u64 = 0;
        let status = unsafe { notify_get_state(token, &mut state) };
        if status != 0 {
            return Err(std::io::Error::other(format!("notify_get_state: {status}")));
        }
        Ok(u32::try_from(state).unwrap_or(u32::MAX))
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
        ProbeHealth::Degraded("coarse: thermal pressure level only (root for SMC)".into())
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
                        "thermal pressure level not available: {e}"
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

    /// #244: the probe read `kern.thermalstate`, which does not exist on
    /// Apple Silicon (`sysctl: unknown oid` on an M2 Pro), so thermal state
    /// was never captured on the only hardware smeltr targets.
    #[test]
    fn read_state_works_on_apple_silicon() {
        let level = read_state().expect("thermal pressure level");
        assert!(level <= 4, "implausible thermal pressure level: {level}");
    }
}

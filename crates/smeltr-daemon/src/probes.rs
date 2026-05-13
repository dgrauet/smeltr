//! Wires per-source probes into the active session + broadcast bus.

use crate::bus::Bus;
use crate::sessions::ActiveSession;
use smeltr_core::event::{Payload, Source};
use smeltr_probes_core::sink::EventSink;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Bridges probe emissions into the active session + broadcast bus.
pub struct DaemonSink {
    pub session: Arc<ActiveSession>,
    pub bus: Bus,
}

impl EventSink for DaemonSink {
    fn emit(&self, source: Source, pid: Option<u32>, payload: Payload) {
        match self.session.append(source, pid, payload) {
            Ok(ev) => {
                self.bus.publish(ev);
            }
            Err(e) => tracing::warn!(error = %e, "session append failed"),
        }
    }
}

pub struct ProbeRuntime {
    handle: tokio::sync::Mutex<Option<smeltr_probes_core::SupervisorHandle>>,
    sink: Arc<DaemonSink>,
    scoped: tokio::sync::Mutex<HashMap<u32, smeltr_probes_core::SupervisorHandle>>,
}

impl ProbeRuntime {
    pub fn start_global(sink: Arc<DaemonSink>) -> Self {
        use smeltr_probes_core::Supervisor;
        let sink_dyn: smeltr_probes_core::SharedSink = sink.clone();
        let mut sup = Supervisor::new(sink_dyn);
        sup.add(Box::new(smeltr_probes_vm::VmProbe::new(
            Duration::from_secs(1),
        )));
        sup.add(Box::new(smeltr_probes_proc::ProcProbe::new(
            Duration::from_secs(2),
            10,
        )));
        sup.add(Box::new(smeltr_probes_thermal::ThermalProbe::new(
            Duration::from_secs(2),
        )));
        sup.add(Box::new(smeltr_probes_oslog::OsLogProbe::new()));
        sup.add(Box::new(smeltr_probes_ioreport::IoReportProbe::new(
            Duration::from_secs(1),
        )));
        sup.add(Box::new(
            smeltr_probes_crash_reports::CrashReportsProbe::new(),
        ));
        Self {
            handle: tokio::sync::Mutex::new(Some(sup.spawn())),
            sink,
            scoped: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    pub async fn attach_scoped(&self, pid: u32) {
        use smeltr_probes_core::Supervisor;
        let sink_dyn: smeltr_probes_core::SharedSink = self.sink.clone();
        let mut sup = Supervisor::new(sink_dyn);
        sup.add(Box::new(
            smeltr_probes_mach_exceptions::MachExceptionsProbe::new(pid),
        ));
        sup.add(Box::new(
            smeltr_probes_crash_reports::CrashReportsProbe::new().filter_pids(vec![pid]),
        ));
        let handle = sup.spawn();
        self.scoped.lock().await.insert(pid, handle);
    }

    pub async fn detach_scoped(&self, pid: u32) {
        let h = self.scoped.lock().await.remove(&pid);
        if let Some(h) = h {
            h.shutdown().await;
        }
    }

    pub async fn shutdown(&self) {
        let mut scoped = std::mem::take(&mut *self.scoped.lock().await);
        for (_, h) in scoped.drain() {
            h.shutdown().await;
        }
        if let Some(h) = self.handle.lock().await.take() {
            h.shutdown().await;
        }
    }
}

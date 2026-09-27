//! Wires per-source probes into the session router.

use crate::session_router::SessionRouter;
use smeltr_core::event::{Payload, Source};
use smeltr_probes_core::sink::EventSink;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Routes probe emissions through the session router.
///
/// `SessionRouter::append` dispatches each event to the correct session
/// (scoped for a known PID, ambient otherwise) and publishes to the bus for
/// sessions that were opened with a `Bus` instance.
pub struct DaemonSink {
    pub router: Arc<SessionRouter>,
}

impl EventSink for DaemonSink {
    fn emit(&self, source: Source, pid: Option<u32>, payload: Payload) {
        if let Err(e) = self.router.append(source, pid, None, payload) {
            tracing::warn!(error = %e, "session append failed");
        }
    }

    fn emit_at(&self, source: Source, pid: Option<u32>, uptime_raw_ns: u64, payload: Payload) {
        if let Err(e) = self.router.append_at(source, pid, uptime_raw_ns, payload) {
            tracing::warn!(error = %e, "session append failed");
        }
    }
}

pub struct ProbeRuntime {
    handle: tokio::sync::Mutex<Option<smeltr_probes_core::SupervisorHandle>>,
    sink: Arc<DaemonSink>,
    scoped: tokio::sync::Mutex<HashMap<u32, smeltr_probes_core::SupervisorHandle>>,
    metal_hooks: tokio::sync::Mutex<HashMap<u32, smeltr_probes_core::SupervisorHandle>>,
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
            smeltr_probes_proc::ProcProbe::default_period(),
            10,
        )));
        sup.add(Box::new(smeltr_probes_thermal::ThermalProbe::new(
            Duration::from_secs(2),
        )));
        sup.add(Box::new(smeltr_probes_oslog::OsLogProbe::new()));
        sup.add(Box::new(smeltr_probes_ioreport::IoReportProbe::new()));
        sup.add(Box::new(
            smeltr_probes_crash_reports::CrashReportsProbe::new(),
        ));
        Self {
            handle: tokio::sync::Mutex::new(Some(sup.spawn())),
            sink,
            scoped: tokio::sync::Mutex::new(HashMap::new()),
            metal_hooks: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// No global probes, for tests of the per-pid attach paths.
    #[cfg(test)]
    fn without_global(sink: Arc<DaemonSink>) -> Self {
        Self {
            handle: tokio::sync::Mutex::new(None),
            sink,
            scoped: tokio::sync::Mutex::new(HashMap::new()),
            metal_hooks: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    pub async fn attach_scoped(&self, pid: u32) {
        use smeltr_probes_core::Supervisor;
        let sink_dyn: smeltr_probes_core::SharedSink = self.sink.clone();
        let mut sup = Supervisor::new(sink_dyn);
        sup.add(Box::new(
            smeltr_probes_mach_exceptions::MachExceptionsProbe::new(pid),
        ));
        // No per-recording crash-reports probe: the global one emits each
        // report under the crashed pid, which the router already sends to
        // this recording. A second watcher only duplicated every report on
        // the bus — two post-mortem sessions per crash (#242).
        sup.add(Box::new(
            smeltr_probes_proc::footprint_probe::FootprintProbe::new(
                pid,
                smeltr_probes_proc::footprint_probe::FootprintProbe::default_period(),
            ),
        ));
        let handle = sup.spawn();
        // A pid recorded twice: stop the previous probes, don't orphan them.
        let replaced = self.scoped.lock().await.insert(pid, handle);
        if let Some(old) = replaced {
            old.shutdown().await;
        }
    }

    pub async fn detach_scoped(&self, pid: u32) {
        let h = self.scoped.lock().await.remove(&pid);
        if let Some(h) = h {
            h.shutdown().await;
        }
    }

    pub async fn attach_metal_hook(&self, pid: u32, ring_path: std::path::PathBuf) {
        use smeltr_probes_core::Supervisor;
        let sink_dyn: smeltr_probes_core::SharedSink = self.sink.clone();
        let mut sup = Supervisor::new(sink_dyn);
        sup.add(Box::new(smeltr_probes_metal_hook::MetalHookProbe::new(
            pid, ring_path,
        )));
        let replaced = self.metal_hooks.lock().await.insert(pid, sup.spawn());
        if let Some(old) = replaced {
            old.shutdown().await;
        }
    }

    pub async fn detach_metal_hook(&self, pid: u32) {
        let h = self.metal_hooks.lock().await.remove(&pid);
        if let Some(h) = h {
            h.shutdown().await;
        }
    }

    pub async fn shutdown(&self) {
        let mut mh = std::mem::take(&mut *self.metal_hooks.lock().await);
        for (_, h) in mh.drain() {
            h.shutdown().await;
        }
        let mut scoped = std::mem::take(&mut *self.scoped.lock().await);
        for (_, h) in scoped.drain() {
            h.shutdown().await;
        }
        if let Some(h) = self.handle.lock().await.take() {
            h.shutdown().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_router::SessionRouter;
    use crate::sessions::ActiveSession;
    use smeltr_core::event::Payload;
    use smeltr_metal_ring::create_ring;

    /// #272: re-attaching the hook for a pid must stop the probe it
    /// replaces; otherwise the old one keeps draining its ring forever.
    /// (Mutation "forget the replaced handle" used to pass every test.)
    #[tokio::test]
    #[serial_test::serial]
    async fn reattaching_a_hook_stops_the_replaced_probe() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let ambient = Arc::new(ActiveSession::open_new().unwrap());
        let router = Arc::new(SessionRouter::new(ambient.clone(), None, None));
        let rt = ProbeRuntime::without_global(Arc::new(DaemonSink {
            router: router.clone(),
        }));
        let old = home.path().join("old.ring");
        let new = home.path().join("new.ring");
        let mut old_writer = create_ring(&old, 1 << 16).unwrap();
        drop(create_ring(&new, 1 << 16).unwrap());

        rt.attach_metal_hook(4242, old.clone()).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        rt.attach_metal_hook(4242, new).await;
        old_writer.write_buffer_free(1, 0xdead).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        rt.detach_metal_hook(4242).await;

        ambient.finalize(Some(0), None, "test").unwrap();
        let dir = smeltr_core::reader::list_sessions().unwrap().remove(0);
        let drained = smeltr_core::reader::read_events(&dir)
            .unwrap()
            .iter()
            .any(|e| matches!(e.payload, Payload::MetalBufferFree { buffer_id: 0xdead }));
        assert!(!drained, "the replaced probe was still draining its ring");
    }
}

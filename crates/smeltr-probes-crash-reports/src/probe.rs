use crate::parse::parse_ips;
use async_trait::async_trait;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use smeltr_core::event::Source;
use smeltr_probes_core::sink::SharedSink;
use smeltr_probes_core::{Probe, ProbeError, ProbeHealth};
use std::path::PathBuf;
use std::sync::mpsc as std_mpsc;
use tokio_util::sync::CancellationToken;

pub struct CrashReportsProbe {
    dirs: Vec<PathBuf>,
    pub pid_filter: Option<Vec<u32>>,
}

impl CrashReportsProbe {
    pub fn new() -> Self {
        let mut dirs = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join("Library/Logs/DiagnosticReports"));
        }
        Self {
            dirs,
            pid_filter: None,
        }
    }
    pub fn with_dirs(dirs: Vec<PathBuf>) -> Self {
        Self {
            dirs,
            pid_filter: None,
        }
    }
    pub fn filter_pids(mut self, pids: Vec<u32>) -> Self {
        self.pid_filter = Some(pids);
        self
    }
}

impl Default for CrashReportsProbe {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Probe for CrashReportsProbe {
    fn name(&self) -> &'static str {
        "crash-reports"
    }
    fn health(&self) -> ProbeHealth {
        ProbeHealth::Ok
    }

    async fn run(&mut self, sink: SharedSink, cancel: CancellationToken) -> Result<(), ProbeError> {
        let (tx, rx) = std_mpsc::channel::<notify::Result<Event>>();
        let mut watcher: RecommendedWatcher =
            notify::recommended_watcher(tx).map_err(|e| ProbeError::Transient(e.to_string()))?;
        for d in &self.dirs {
            if d.exists() {
                watcher
                    .watch(d, RecursiveMode::NonRecursive)
                    .map_err(|e| ProbeError::Transient(format!("watch {d:?}: {e}")))?;
            }
        }
        let pid_filter = self.pid_filter.clone();
        loop {
            if cancel.is_cancelled() {
                return Ok(());
            }
            tokio::task::yield_now().await;
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(Ok(ev)) => {
                    if !matches!(ev.kind, EventKind::Create(_) | EventKind::Modify(_)) {
                        continue;
                    }
                    for p in &ev.paths {
                        if p.extension().and_then(|s| s.to_str()) != Some("ips") {
                            continue;
                        }
                        let content = match std::fs::read_to_string(p) {
                            Ok(c) => c,
                            Err(_) => continue,
                        };
                        if let Some(payload) = parse_ips(&content, &p.to_string_lossy()) {
                            if let Some(filter) = &pid_filter {
                                if let smeltr_core::event::Payload::CrashReportEmitted {
                                    crashed_pid,
                                    ..
                                } = &payload
                                {
                                    if let Some(pid) = crashed_pid {
                                        if !filter.contains(pid) {
                                            continue;
                                        }
                                    } else {
                                        continue;
                                    }
                                }
                            }
                            sink.emit(Source::CrashReport, None, payload);
                        }
                    }
                }
                Ok(Err(e)) => tracing::warn!("watcher error: {e}"),
                Err(std_mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ProbeError::Transient("watcher disconnected".into()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_probes_core::sink::test_util::CapturingSink;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn detects_ips_file_drop_in_watched_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let probe = CrashReportsProbe::with_dirs(vec![dir.clone()]);
        let sink: Arc<CapturingSink> = Arc::default();
        let token = CancellationToken::new();
        let sink_dyn: SharedSink = sink.clone();
        let token2 = token.clone();
        let h = tokio::spawn(async move {
            let mut p = probe;
            p.run(sink_dyn, token2).await
        });

        tokio::time::sleep(Duration::from_millis(600)).await;
        let fixture = include_str!("../tests/fixtures/sample.ips");
        std::fs::write(dir.join("python-2026-05-13.ips"), fixture).unwrap();

        tokio::time::sleep(Duration::from_millis(1500)).await;
        token.cancel();
        let _ = h.await;

        let evs = sink.events.lock().unwrap();
        assert!(
            evs.iter()
                .any(|(src, _, _)| matches!(src, Source::CrashReport)),
            "no CrashReport events seen, got {} events",
            evs.len()
        );
    }
}

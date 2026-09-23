use crate::parse::{incident_id, parse_ips};
use async_trait::async_trait;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use smeltr_core::event::Source;
use smeltr_probes_core::sink::SharedSink;
use smeltr_probes_core::{Probe, ProbeError, ProbeHealth};
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

/// Process whose crash reports the daemon declines to ingest: a crashed
/// smeltrd already writes its own `post-mortem-daemon-panic-*` session from
/// its panic hook, carrying the panic message and backtrace the `.ips` does
/// not have. Ingesting the report on top only adds a poorer duplicate --
/// and, since our own test suite aborts daemons on purpose, one per
/// `cargo test` run on the developer's machine (#227).
///
/// The cost, deliberately accepted: an smeltrd killed outside the panic hook
/// (SIGSEGV, SIGKILL) gets no post-mortem session. The report stays on disk
/// and `smeltr analyze` still joins it (`crash_join.rs`).
const SELF_PROC_NAME: &str = "smeltrd";

/// Upper bound on remembered incidents. Crash reports are rare; this only
/// exists so a daemon running for weeks cannot grow the set without limit.
const SEEN_CAP: usize = 512;

/// Remembers which crash reports have already been emitted.
///
/// Keyed on the report's incident UUID, falling back to its path when the
/// header cannot be read. Nothing is recorded for a report that failed to
/// parse, so the self-healing re-parse of a partial write (#151) still emits
/// the first time it succeeds.
#[derive(Default)]
struct SeenReports {
    keys: HashSet<String>,
    order: VecDeque<String>,
}

impl SeenReports {
    /// Records `key` and reports whether it is new.
    fn insert(&mut self, key: String) -> bool {
        if !self.keys.insert(key.clone()) {
            return false;
        }
        self.order.push_back(key);
        if self.order.len() > SEEN_CAP {
            if let Some(old) = self.order.pop_front() {
                self.keys.remove(&old);
            }
        }
        true
    }
}

pub struct CrashReportsProbe {
    dirs: Vec<PathBuf>,
    /// Signalled once the directories are watched (tests: FSEvents takes
    /// seconds to start, and writes made before that are never seen).
    #[cfg(test)]
    ready: Option<tokio::sync::oneshot::Sender<()>>,
}

impl CrashReportsProbe {
    pub fn new() -> Self {
        let mut dirs = Vec::new();
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(PathBuf::from(home).join("Library/Logs/DiagnosticReports"));
        }
        Self::with_dirs(dirs)
    }
    pub fn with_dirs(dirs: Vec<PathBuf>) -> Self {
        Self {
            dirs,
            #[cfg(test)]
            ready: None,
        }
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
        // A tokio channel, awaited: waiting on a std channel inside this async
        // fn held its worker thread (#244).
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Result<Event>>();
        // Starting (and stopping) an FSEvents stream blocks for seconds —
        // `watch()` measured at 1.4-5.7 s: keep both off the async workers.
        let dirs = self.dirs.clone();
        let watcher = tokio::task::spawn_blocking(move || {
            let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res| {
                let _ = tx.send(res);
            })
            .map_err(|e| ProbeError::Transient(e.to_string()))?;
            for d in &dirs {
                if d.exists() {
                    watcher
                        .watch(d, RecursiveMode::NonRecursive)
                        .map_err(|e| ProbeError::Transient(format!("watch {d:?}: {e}")))?;
                }
            }
            Ok::<_, ProbeError>(watcher)
        })
        .await
        .map_err(|e| ProbeError::Transient(e.to_string()))??;
        #[cfg(test)]
        if let Some(ready) = self.ready.take() {
            let _ = ready.send(());
        }
        let result = self.drain(&mut rx, &sink, &cancel).await;
        tokio::task::spawn_blocking(move || drop(watcher))
            .await
            .ok();
        result
    }
}

impl CrashReportsProbe {
    async fn drain(
        &self,
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<notify::Result<Event>>,
        sink: &SharedSink,
        cancel: &CancellationToken,
    ) -> Result<(), ProbeError> {
        let mut seen = SeenReports::default();
        loop {
            let msg = tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                msg = rx.recv() => msg,
            };
            match msg {
                Some(Ok(ev)) => {
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
                        let parsed = parse_ips(&content, &p.to_string_lossy());
                        if parsed.is_none() {
                            // A crash report we cannot parse is exactly the
                            // moment observability must not be silent (#151).
                            // Partial reads self-heal: ReportCrash keeps
                            // writing and the next Modify event re-parses.
                            tracing::warn!(path = %p.display(), "failed to parse .ips crash report (partial write or unknown format)");
                        }
                        if let Some(payload) = parsed {
                            if let smeltr_core::event::Payload::CrashReportEmitted {
                                proc_name: Some(name),
                                ..
                            } = &payload
                            {
                                if name == SELF_PROC_NAME {
                                    continue;
                                }
                            }
                            // Key on the incident, not the event: ReportCrash
                            // rewrites a report several times and each pass is
                            // its own notify event (#227).
                            let key = incident_id(&content)
                                .unwrap_or_else(|| p.to_string_lossy().into_owned());
                            if !seen.insert(key) {
                                continue;
                            }
                            // Emitted as the crashed process: the daemon routes
                            // by pid, so the report joins that process's
                            // recording when it is still open, and the ambient
                            // session otherwise (#242).
                            let crashed_pid = match &payload {
                                smeltr_core::event::Payload::CrashReportEmitted {
                                    crashed_pid,
                                    ..
                                } => *crashed_pid,
                                _ => None,
                            };
                            sink.emit(Source::CrashReport, crashed_pid, payload);
                        }
                    }
                }
                Some(Err(e)) => tracing::warn!("watcher error: {e}"),
                None => {
                    return Err(ProbeError::Transient("watcher disconnected".into()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::Payload;
    use smeltr_probes_core::sink::test_util::CapturingSink;
    use std::sync::Arc;
    use std::time::Duration;

    const FIXTURE: &str = include_str!("../tests/fixtures/sample.ips");

    /// Runs the probe over a temp dir, replays `writes` into it one after the
    /// other, and returns every CrashReport payload the probe emitted.
    ///
    /// Each write is a separate filesystem event: writing the same name twice
    /// is exactly the Create-then-Modify sequence ReportCrash produces.
    async fn emitted_for(writes: &[(&str, &str)]) -> Vec<Payload> {
        emitted_with_pid_for(writes)
            .await
            .into_iter()
            .map(|(_, payload)| payload)
            .collect()
    }

    /// Like [`emitted_for`], keeping the pid each event was emitted with.
    async fn emitted_with_pid_for(writes: &[(&str, &str)]) -> Vec<(Option<u32>, Payload)> {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let mut probe = CrashReportsProbe::with_dirs(vec![dir.clone()]);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        probe.ready = Some(ready_tx);
        let sink: Arc<CapturingSink> = Arc::default();
        let token = CancellationToken::new();
        let sink_dyn: SharedSink = sink.clone();
        let token2 = token.clone();
        let h = tokio::spawn(async move {
            let mut p = probe;
            p.run(sink_dyn, token2).await
        });

        tokio::time::timeout(Duration::from_secs(30), ready_rx)
            .await
            .expect("watcher never started")
            .unwrap();
        for (name, content) in writes {
            std::fs::write(dir.join(name), content).unwrap();
            tokio::time::sleep(Duration::from_millis(700)).await;
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
        token.cancel();
        let _ = h.await;

        let evs = sink.events.lock().unwrap();
        evs.iter()
            .filter(|(src, _, _)| matches!(src, Source::CrashReport))
            .map(|(_, pid, payload)| (*pid, payload.clone()))
            .collect()
    }

    /// #242: emitted with the crashed pid, so the daemon routes the report
    /// into that process's recording while it is still open — with `None`
    /// every report landed in the ambient session.
    #[tokio::test]
    async fn emits_with_the_crashed_pid() {
        let evs = emitted_with_pid_for(&[("python-2026-05-13.ips", FIXTURE)]).await;
        assert_eq!(evs.len(), 1, "got {evs:?}");
        assert_eq!(evs[0].0, Some(38291));
    }

    /// #244: the probe waited on a std channel (`recv_timeout(200 ms)`)
    /// inside its async fn, holding the worker thread. On a single-threaded
    /// runtime, a task beside it must keep its 10 ms cadence.
    #[tokio::test(flavor = "current_thread")]
    async fn probe_does_not_block_its_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let probe = CrashReportsProbe::with_dirs(vec![tmp.path().to_path_buf()]);
        let sink: Arc<CapturingSink> = Arc::default();
        let token = CancellationToken::new();
        let (sink_dyn, token2): (SharedSink, _) = (sink.clone(), token.clone());
        let h = tokio::spawn(async move {
            let mut p = probe;
            p.run(sink_dyn, token2).await
        });
        let start = std::time::Instant::now();
        let mut ticks = 0;
        while start.elapsed() < Duration::from_millis(500) {
            tokio::time::sleep(Duration::from_millis(10)).await;
            ticks += 1;
        }
        token.cancel();
        let _ = h.await;
        assert!(ticks >= 25, "only {ticks} ticks of 10 ms in 500 ms");
    }

    #[tokio::test]
    async fn detects_ips_file_drop_in_watched_dir() {
        let evs = emitted_for(&[("python-2026-05-13.ips", FIXTURE)]).await;
        assert_eq!(evs.len(), 1, "expected exactly one report, got {evs:?}");
    }

    #[tokio::test]
    async fn same_report_rewritten_emits_once() {
        // ReportCrash writes a report in several passes; every pass is a
        // notify event and re-parsing is deliberate (#151). What must not
        // repeat is the emission -- each one drives its own post-mortem
        // session (#227).
        let evs = emitted_for(&[
            ("python-2026-05-13.ips", FIXTURE),
            ("python-2026-05-13.ips", FIXTURE),
            ("python-2026-05-13.ips", FIXTURE),
        ])
        .await;
        assert_eq!(evs.len(), 1, "expected exactly one report, got {evs:?}");
    }

    #[tokio::test]
    async fn partial_then_complete_write_emits_once() {
        // The self-healing path: the first pass cannot be parsed, so nothing
        // is remembered and the completed report still gets through -- once.
        let half = &FIXTURE[..FIXTURE.len() / 2];
        let evs = emitted_for(&[
            ("python-2026-05-13.ips", half),
            ("python-2026-05-13.ips", FIXTURE),
        ])
        .await;
        assert_eq!(evs.len(), 1, "expected exactly one report, got {evs:?}");
    }

    #[tokio::test]
    async fn distinct_incidents_both_emit() {
        let other = FIXTURE.replacen("ABC123", "DEF456", 1);
        assert_ne!(other, FIXTURE, "fixture must carry an incident id");
        let evs = emitted_for(&[
            ("python-2026-05-13.ips", FIXTURE),
            ("python-2026-05-14.ips", &other),
        ])
        .await;
        assert_eq!(evs.len(), 2, "expected two reports, got {evs:?}");
    }

    #[tokio::test]
    async fn reports_without_incident_id_fall_back_to_path() {
        // A header we cannot read the incident from must not collapse every
        // such report into one: the path keeps them apart, while repeated
        // writes of the same path are still deduplicated.
        let anon = FIXTURE.replacen("\"incident_id\"", "\"unknown_key\"", 1);
        assert!(incident_id(&anon).is_none());
        let evs = emitted_for(&[("a.ips", &anon), ("a.ips", &anon), ("b.ips", &anon)]).await;
        assert_eq!(evs.len(), 2, "expected two reports, got {evs:?}");
    }

    #[tokio::test]
    async fn own_daemon_crash_report_is_skipped() {
        // A crashed smeltrd already writes its own post-mortem session, with
        // the panic message and backtrace the .ips does not carry (#227).
        let daemon = include_str!("../tests/fixtures/smeltrd.ips");
        let evs = emitted_for(&[("smeltrd-2026-08-17-123523.ips", daemon)]).await;
        assert!(evs.is_empty(), "expected no report, got {evs:?}");
    }
}

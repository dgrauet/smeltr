//! Analyze-time crash-report join (#153).
//!
//! ReportCrash writes the `.ips` seconds AFTER the crashed process dies —
//! by then the scoped session is already finalized (the record client's
//! connection dropped, #143), so the live crash-reports probe cannot land
//! the report in the crashed session. This module joins retroactively:
//! given the crashed session's child pid and wall-clock window, it scans
//! the DiagnosticReports directory for a matching report and turns it
//! into a RootCause finding. Works on sessions recorded before the fix.

use crate::finding::{Category, EvidenceRef, Finding, Severity};
use smeltr_core::event::{Event, Payload};
use smeltr_probes_crash_reports::parse::parse_ips;
use std::path::Path;
use std::time::UNIX_EPOCH;

/// How long after the session end a report may be written and still be
/// attributed to it. ReportCrash typically takes seconds; sleep/wake and
/// symbolication can stretch that.
pub const CRASH_REPORT_GRACE_NS: u64 = 120_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashJoin {
    pub path: String,
    pub crashed_pid: u32,
    pub signal: Option<String>,
    pub summary: String,
    pub exception_codes: Vec<String>,
}

/// Scan `reports_dir` for a `.ips` whose crashed pid matches `pid` and
/// whose mtime falls inside `[wall_start_ns, wall_end_ns + grace_ns]`
/// (unix wall-clock ns). Returns the newest match.
pub fn find_crash_report(
    reports_dir: &Path,
    pid: u32,
    wall_start_ns: u64,
    wall_end_ns: u64,
    grace_ns: u64,
) -> Option<CrashJoin> {
    let entries = std::fs::read_dir(reports_dir).ok()?;
    let mut best: Option<(u64, CrashJoin)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("ips") {
            continue;
        }
        let mtime_ns = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64);
        let Some(mtime_ns) = mtime_ns else { continue };
        if mtime_ns < wall_start_ns || mtime_ns > wall_end_ns.saturating_add(grace_ns) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(Payload::CrashReportEmitted {
            path: p,
            crashed_pid,
            signal,
            exception_codes,
            summary,
            ..
        }) = parse_ips(&content, &path.to_string_lossy())
        else {
            continue;
        };
        if crashed_pid != Some(pid) {
            continue;
        }
        let join = CrashJoin {
            path: p,
            crashed_pid: pid,
            signal,
            summary,
            exception_codes,
        };
        match &best {
            Some((t, _)) if *t >= mtime_ns => {}
            _ => best = Some((mtime_ns, join)),
        }
    }
    best.map(|(_, j)| j)
}

/// Turn a joined crash report into a RootCause finding for the report.
pub fn crash_finding(j: &CrashJoin) -> Finding {
    let title = match &j.signal {
        Some(sig) => format!("Recorded process crashed ({sig})"),
        None => "Recorded process crashed".to_string(),
    };
    let mut detail = String::new();
    if !j.summary.is_empty() {
        detail.push_str(&j.summary);
    }
    if !j.exception_codes.is_empty() {
        if !detail.is_empty() {
            detail.push_str(" — ");
        }
        detail.push_str(&format!("codes: {}", j.exception_codes.join(", ")));
    }
    if !detail.is_empty() {
        detail.push_str("\n    ");
    }
    detail.push_str(&format!("crash report: {}", j.path));
    Finding::new(Severity::Critical, Category::RootCause, title).with_detail(detail)
}

/// A jetsam kill joined retroactively to the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JetsamJoin {
    pub path: String,
    pub killed_pid: u32,
    pub killed_name: String,
    pub footprint_bytes: u64,
    pub lifetime_max_bytes: u64,
    /// Reason as the kernel reports it, when it gives one.
    pub reason: Option<String>,
}

/// Directories where macOS drops its diagnostic reports.
///
/// The SYSTEM directory comes first: that is where `JetsamEvent-*.ips` files
/// are written, unlike ordinary crash reports which land in the user
/// directory. Neither the probe nor the crash join used to look at the system
/// directory — without it the feature never fires.
///
/// `SMELTR_DIAGNOSTIC_REPORTS_DIR` replaces the whole list, for tests.
pub fn diagnostic_reports_dirs() -> Vec<std::path::PathBuf> {
    if let Some(over) = std::env::var_os("SMELTR_DIAGNOSTIC_REPORTS_DIR") {
        return vec![std::path::PathBuf::from(over)];
    }
    let mut dirs = vec![std::path::PathBuf::from("/Library/Logs/DiagnosticReports")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(std::path::PathBuf::from(home).join("Library/Logs/DiagnosticReports"));
    }
    dirs
}

/// Do two process names plausibly designate the same process?
///
/// PREFIX comparison, not equality: the two sides truncate differently.
/// `pbi_comm` (where `ProcFootprint` names come from) is 16 bytes —
/// `MAXCOMLEN` — while a jetsam report's `name` runs up to ~32 (observed on
/// this machine: `"com.apple.Virtualization.Virtual"`, 32 characters).
/// Requiring equality would reject precisely the long names.
///
/// An empty name carries no information and therefore proves nothing.
pub fn names_compatible(a: &str, b: &str) -> bool {
    let n = a.len().min(b.len());
    n > 0 && a.as_bytes()[..n] == b.as_bytes()[..n]
}

/// Searches `dirs` for a jetsam report naming one of `pids`, whose mtime falls
/// within `[wall_start_ns, wall_end_ns + grace_ns]`. Returns the most recent.
///
/// `pids` holds the scoped PID AND every PID seen in the footprint samples:
/// under `uv run` / `poetry run` / `python -m`, the process that dies is a
/// grandchild with a different PID (#31).
///
/// The triple filter PID + window + name is what prevents attributing some
/// unrelated process's jetsam kill to the run under analysis. `known_names`
/// lists the traced process's known names; when empty the name guard does not
/// apply and we fall back to PID + window.
pub fn find_jetsam_report(
    dirs: &[std::path::PathBuf],
    pids: &[u32],
    known_names: &[String],
    wall_start_ns: u64,
    wall_end_ns: u64,
    grace_ns: u64,
) -> Option<JetsamJoin> {
    let mut best: Option<(u64, JetsamJoin)> = None;
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("ips") {
                continue;
            }
            let mtime_ns = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as u64);
            let Some(mtime_ns) = mtime_ns else { continue };
            if mtime_ns < wall_start_ns || mtime_ns > wall_end_ns.saturating_add(grace_ns) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(Payload::JetsamKill {
                path: p,
                killed_pid,
                killed_name,
                footprint_bytes,
                lifetime_max_bytes,
                reason,
                ..
            }) = parse_ips(&content, &path.to_string_lossy())
            else {
                continue;
            };
            let Some(matched_pid) = killed_pid.filter(|k| pids.contains(k)) else {
                continue;
            };
            // Name guard: the PID alone is not enough, macOS recycles them.
            // It applies only when BOTH sides give a name — rejecting
            // otherwise would miss the very kill we are looking for.
            if !known_names.is_empty()
                && !killed_name.is_empty()
                && !known_names
                    .iter()
                    .any(|n| names_compatible(n, &killed_name))
            {
                continue;
            }
            let join = JetsamJoin {
                path: p,
                killed_pid: matched_pid,
                killed_name,
                footprint_bytes,
                lifetime_max_bytes,
                reason,
            };
            match &best {
                Some((t, _)) if *t >= mtime_ns => {}
                _ => best = Some((mtime_ns, join)),
            }
        }
    }
    best.map(|(_, j)| j)
}

/// Transforme un kill joint en cause racine.
pub fn jetsam_finding(j: &JetsamJoin) -> Finding {
    // Decimal GB (1e9), not binary GiB — consistent with how macOS and the
    // jetsam reports themselves express memory footprints.
    let gb = smeltr_core::fmt::decimal_gb;
    // `per-process-limit` and `vm-pageshortage` call for opposite fixes:
    // shrink the run's footprint versus free up the machine. The reason sits
    // in the report; withholding it throws away the answer to the very
    // question this feature exists to settle.
    let reason = match j.reason.as_deref() {
        Some("per-process-limit") => " Reason: `per-process-limit` — the process exceeded ITS OWN \
             limit, regardless of the state of the rest of the machine."
            .to_string(),
        Some("vm-pageshortage") => " Reason: `vm-pageshortage` — the whole machine was short of \
             memory; the run is not necessarily at fault."
            .to_string(),
        Some(other) => format!(" Reason as the kernel reports it: `{other}`."),
        None => String::new(),
    };
    Finding::new(
        Severity::Critical,
        Category::RootCause,
        "The kernel killed the recorded process under memory pressure (jetsam)",
    )
    .with_detail(format!(
        "jetsam killed PID {} ({}) at a footprint of {:.2} GB (lifetime \
         maximum {:.2} GB).{} `phys_footprint` is what decides, not MTLDevice \
         memory: a run can fit within the GPU budget and still be killed. The \
         process vanishes with no traceback and no exception — this finding is \
         the only trace of the decision.\n    report: {}",
        j.killed_pid,
        j.killed_name,
        gb(j.footprint_bytes),
        gb(j.lifetime_max_bytes),
        reason,
        j.path
    ))
}

/// Joins a possible jetsam kill to the report, at the head of the findings.
///
/// Called both by `smeltr analyze` and by the MCP `get_session_summary`: a
/// number you cannot query from a Claude session is useless. No effect on
/// ambient sessions (no PID to join on).
pub fn join_jetsam(report: &mut crate::report::Report, dir: &Path) {
    let Ok(meta) = smeltr_core::reader::read_metadata(dir) else {
        return;
    };
    let smeltr_core::session::SessionKind::Scoped { pid, argv } = &meta.kind else {
        return;
    };
    let Some(start_ns) = rfc3339_unix_ns(&meta.started_rfc3339) else {
        return;
    };
    let events = smeltr_core::reader::read_events(dir).unwrap_or_default();

    let end_ns = window_end_ns(&meta, &events);

    // Candidate PIDs: the scoped PID PLUS every PID seen in the footprint
    // samples, which cover the whole traced tree. Under `uv run` /
    // `poetry run` / `python -m` — this project's normal flow — the process
    // that dies is a grandchild whose PID differs from the spawned child's
    // (#31): sticking to the scoped PID produced silence in the very case the
    // feature targets.
    //
    // Known names, for the name guard: the basename of argv[0] plus every name
    // seen in those same samples.
    let mut pids: Vec<u32> = vec![*pid];
    let mut known_names: Vec<String> = argv
        .first()
        .and_then(|a| a.rsplit('/').next())
        .filter(|s| !s.is_empty())
        .map(|s| vec![s.to_string()])
        .unwrap_or_default();
    for e in &events {
        if let Payload::ProcFootprint {
            pid: sample_pid,
            name,
            ..
        } = &e.payload
        {
            if !pids.contains(sample_pid) {
                pids.push(*sample_pid);
            }
            if !name.is_empty() && !known_names.iter().any(|n| n == name) {
                known_names.push(name.clone());
            }
        }
    }

    // Deliberately NO guard on meta.exit_code (unlike the crash join): a
    // process killed by jetsam has no clean exit code, but the parent shell
    // may report one — we do not want to miss the kill over that. The triple
    // filter PID + window + name is enough to avoid false positives.
    if let Some(j) = find_jetsam_report(
        &diagnostic_reports_dirs(),
        &pids,
        &known_names,
        start_ns,
        end_ns,
        CRASH_REPORT_GRACE_NS,
    ) {
        report.findings.insert(0, jetsam_finding(&j));
        return;
    }

    // No joinable report: the presumption remains, and only here. The
    // suppression is structural — this point is reached only for want of a
    // verdict — rather than a separate check somebody could forget.
    if let Some(f) = presume_memory_death(&meta, &events, report) {
        report.findings.push(f);
    }
}

/// Upper bound of a session's wall-clock window, for joining a report written
/// after the fact.
///
/// A kill (jetsam or crash alike) often stops the `record` client from
/// finalizing cleanly (#143): lacking `ended_rfc3339`, we bound on the last
/// event written. Letting it run to NOW would make the window weeks wide on an
/// old unfinalized session — leaving only a PID, which macOS recycles, to hold
/// the verdict. Fall back to now only when there is no event to date.
fn window_end_ns(meta: &smeltr_core::session::SessionMetadata, events: &[Event]) -> u64 {
    meta.ended_rfc3339
        .as_deref()
        .and_then(rfc3339_unix_ns)
        .or_else(|| {
            events
                .iter()
                .map(|e| e.ts_wall_ns)
                .max()
                .map(|t| t.saturating_add(CRASH_REPORT_GRACE_NS))
        })
        .unwrap_or_else(|| time::OffsetDateTime::now_utc().unix_timestamp_nanos() as u64)
}

/// Joins a possible crash report to the session, at the head of the findings.
///
/// Twin of [`join_jetsam`], called from `smeltr analyze` **and** from the MCP
/// `get_session_summary`. Before #204 this work lived in `analyze.rs` and thus
/// existed only in the CLI: the MCP never surfaced a crash verdict, and since
/// #201 it substituted the memory-death presumption for it — saying something
/// inaccurate being worse than staying silent.
///
/// The window goes through [`window_end_ns`], so a session that was never
/// finalized is covered too; the old block required `ended_rfc3339` and
/// silently skipped that case, in the CLI as well.
pub fn join_crash(report: &mut crate::report::Report, dir: &Path) {
    let Ok(meta) = smeltr_core::reader::read_metadata(dir) else {
        return;
    };
    let smeltr_core::session::SessionKind::Scoped { pid, .. } = &meta.kind else {
        return;
    };
    // A clean exit is not a crash. This guard is deliberately absent from
    // `join_jetsam` (a jetsam kill can still let the shell report a clean
    // code), but legitimate here: ReportCrash writes nothing on an exit 0.
    if meta.exit_code == Some(0) {
        return;
    }
    let Some(start_ns) = rfc3339_unix_ns(&meta.started_rfc3339) else {
        return;
    };
    let events = smeltr_core::reader::read_events(dir).unwrap_or_default();
    let end_ns = window_end_ns(&meta, &events);

    for reports_dir in diagnostic_reports_dirs() {
        if let Some(j) =
            find_crash_report(&reports_dir, *pid, start_ns, end_ns, CRASH_REPORT_GRACE_NS)
        {
            report.findings.insert(0, crash_finding(&j));
            return;
        }
    }
}

/// Stop signals requested by the user or by a supervisor. Hard-coded rather
/// than imported from `libc`: the analyzer reads sessions that may come from
/// another machine, and both numbers are fixed by POSIX.
const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

/// Minimum growth between the first and last sample to qualify as a slope. A
/// factor, not an absolute threshold: we do not pretend to know at what size
/// the kernel strikes, and the per-process jetsam limit is not queryable
/// without special privileges anyway.
const RISE_FACTOR: f64 = 2.0;

/// Presumes a memory-pressure death when no report can be joined.
///
/// Three conditions, all necessary:
///
/// 1. **The session ended, and ended abnormally.** The exit code says so, not
///    the event stream: `finalize()` writes `SessionEnded` even when the child
///    was killed, because the `record` client survives the kill and detaches
///    cleanly (#201). `Some(-1)` comes from `status.code().unwrap_or(-1)` and
///    means "killed by a signal". A clean exit (`Some(0)`) and an ordinary
///    failure (`Some(1)`) are both excluded.
///
///    The `None` case needs an extra guard. It covers two shapes of abnormal
///    ending — the daemon finalized on client disconnect, or boot recovery
///    caught an orphaned session — but also a session **still running**, whose
///    exit code does not exist yet. Without the distinction, a `smeltr analyze
///    --last` during a long run would announce an abnormal ending on a session
///    that is doing fine. Every finalization path writes `ended_rfc3339`:
///    `finalize()` sets it along with the exit code (`writer.rs`), and boot
///    recovery sets it too (`recovery.rs`). Its absence therefore means
///    exactly "session alive", and that is what we exclude.
/// 2. **The footprint was rising.** The first sample is the probe's very first
///    tick, before the child has exec'd its workload, so the slope alone
///    proves nothing — hence condition 1.
/// 3. **No root cause is already established.** A SIGSEGV also yields `-1`; if
///    one was joined upstream, the verdict outranks the guess.
fn presume_memory_death(
    meta: &smeltr_core::session::SessionMetadata,
    events: &[Event],
    report: &crate::report::Report,
) -> Option<Finding> {
    // A user-requested stop is not a memory death. Ctrl-C sends SIGINT to the
    // whole foreground process group, and a long run always clears the growth
    // factor: without this guard, the normal gesture for interrupting an
    // inference would produce a memory-pressure presumption (#203).
    if matches!(meta.term_signal, Some(SIGINT) | Some(SIGTERM)) {
        return None;
    }
    let ended_abnormally = match meta.exit_code {
        Some(-1) => true,
        // No exit code: abnormal ending only if the session really has ended.
        // Otherwise it is still running — see above.
        None => meta.ended_rfc3339.is_some(),
        _ => false,
    };
    if !ended_abnormally {
        return None;
    }
    if report
        .findings
        .iter()
        .any(|f| f.category == Category::RootCause)
    {
        return None;
    }

    let samples: Vec<(&Event, u64)> = events
        .iter()
        .filter_map(|e| match &e.payload {
            Payload::ProcFootprint {
                phys_footprint_bytes,
                is_traced_root: true,
                ..
            } => Some((e, *phys_footprint_bytes)),
            _ => None,
        })
        .collect();
    let (first_ev, first) = samples.first().copied()?;
    let (last_ev, last) = samples.last().copied()?;
    if first == 0 || (last as f64) < (first as f64) * RISE_FACTOR {
        return None;
    }

    // Decimal GB (1e9), like `jetsam_finding`: macOS and the jetsam reports
    // express footprints that way, and two findings about the same fact in two
    // different units do not compare.
    let gb = smeltr_core::fmt::decimal_gb;
    Some(
        Finding::new(
            Severity::Warning,
            Category::ContributingFactor,
            "Memory footprint was rising and the session ended abnormally",
        )
        .with_detail(format!(
            "The traced process's footprint went from {:.2} to {:.2} GB, and \
             the session did not end cleanly, with no jetsam report joinable. \
             A memory-pressure death is possible: the process then vanishes \
             with no traceback and no exception. Check \
             /Library/Logs/DiagnosticReports for a JetsamEvent-*.ips later \
             than the session — it can be written after the fact. This is a \
             presumption, not a verdict.",
            gb(first),
            gb(last)
        ))
        .with_evidence(EvidenceRef {
            seq: first_ev.seq,
            ts_mono_ns: first_ev.ts_mono_ns,
            description: format!("initial footprint {:.2} GB", gb(first)),
        })
        .with_evidence(EvidenceRef {
            seq: last_ev.seq,
            ts_mono_ns: last_ev.ts_mono_ns,
            description: format!("last footprint {:.2} GB", gb(last)),
        }),
    )
}

/// Same parsing as `analyze.rs`: metadata timestamps are true wall-clock,
/// unlike the events' `ts_wall_ns` which derive from the monotonic clock and
/// stop during sleep (#153).
pub fn rfc3339_unix_ns(s: &str) -> Option<u64> {
    use time::format_description::well_known::Rfc3339;
    let t = time::OffsetDateTime::parse(s, &Rfc3339).ok()?;
    u64::try_from(t.unix_timestamp_nanos()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTILINE: &str =
        include_str!("../../smeltr-probes-crash-reports/tests/fixtures/sample_multiline.ips");

    /// Window around the fixture file's mtime (files are written by the
    /// test itself, so mtime is "now").
    fn window_around(path: &Path) -> (u64, u64) {
        let mtime = std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        (mtime.saturating_sub(60_000_000_000), mtime)
    }

    #[test]
    fn joins_matching_pid_and_window() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("Python-2026-07-16-213821.ips");
        std::fs::write(&f, MULTILINE).unwrap();
        let (start, end) = window_around(&f);
        let j = find_crash_report(tmp.path(), 11672, start, end, CRASH_REPORT_GRACE_NS)
            .expect("no join");
        assert_eq!(j.crashed_pid, 11672);
        assert_eq!(j.signal.as_deref(), Some("SIGABRT"));
        assert!(j.summary.contains("EXC_CRASH"));
    }

    #[test]
    fn pid_mismatch_yields_none() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("Python.ips");
        std::fs::write(&f, MULTILINE).unwrap();
        let (start, end) = window_around(&f);
        assert!(find_crash_report(tmp.path(), 999, start, end, CRASH_REPORT_GRACE_NS).is_none());
    }

    #[test]
    fn report_outside_window_plus_grace_yields_none() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("Python.ips");
        std::fs::write(&f, MULTILINE).unwrap();
        // Session ended an hour before the report was written.
        let (start, end) = window_around(&f);
        let (old_start, old_end) = (
            start.saturating_sub(3_600_000_000_000),
            end.saturating_sub(3_600_000_000_000),
        );
        assert!(
            find_crash_report(tmp.path(), 11672, old_start, old_end, CRASH_REPORT_GRACE_NS)
                .is_none()
        );
    }

    #[test]
    fn missing_dir_yields_none() {
        assert!(
            find_crash_report(Path::new("/nonexistent-dir-xyz"), 11672, 0, u64::MAX / 2, 0)
                .is_none()
        );
    }

    const JETSAM: &str =
        include_str!("../../smeltr-probes-crash-reports/tests/fixtures/jetsam.ips");

    #[test]
    fn joins_jetsam_report_by_pid_and_window() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();
        let (start, end) = window_around(&f);
        let j = find_jetsam_report(
            &[tmp.path().to_path_buf()],
            &[4242],
            &[],
            start,
            end,
            CRASH_REPORT_GRACE_NS,
        )
        .expect("no join");
        assert_eq!(j.killed_pid, 4242);
        assert_eq!(j.killed_name, "python");
        // 1,310,720 pages * 16 KB = 21.47 GB — the signature of ltx-2-mlx #74.
        assert_eq!(j.footprint_bytes, 1_310_720 * 16_384);
    }

    /// The guard against a false root cause: ANOTHER process's jetsam kill must
    /// never be attributed to the run under analysis.
    #[test]
    fn does_not_join_jetsam_report_of_another_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();
        let (start, end) = window_around(&f);
        assert!(find_jetsam_report(
            &[tmp.path().to_path_buf()],
            &[999_999],
            &[],
            start,
            end,
            CRASH_REPORT_GRACE_NS
        )
        .is_none());
    }

    /// Second guard: outside the wall-clock window, no join.
    #[test]
    fn does_not_join_jetsam_report_outside_window() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();
        let (start, _end) = window_around(&f);
        // Window closed well before the file was written.
        assert!(find_jetsam_report(
            &[tmp.path().to_path_buf()],
            &[4242],
            &[],
            start.saturating_sub(600_000_000_000),
            start.saturating_sub(300_000_000_000),
            0
        )
        .is_none());
    }

    /// An ordinary crash report is not a jetsam report.
    #[test]
    fn regular_crash_report_is_not_joined_as_jetsam() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("Python-2026-07-16-213821.ips");
        std::fs::write(&f, MULTILINE).unwrap();
        let (start, end) = window_around(&f);
        assert!(find_jetsam_report(
            &[tmp.path().to_path_buf()],
            &[11672],
            &[],
            start,
            end,
            CRASH_REPORT_GRACE_NS
        )
        .is_none());
    }

    /// The SYSTEM directory must be in the list: that is where macOS writes
    /// JetsamEvent-*.ips. Verified on the machine: 0 jetsam files in
    /// ~/Library/Logs/DiagnosticReports, 1 in /Library/Logs/DiagnosticReports.
    #[test]
    #[serial_test::serial]
    fn jetsam_dirs_include_the_system_directory() {
        let dirs = diagnostic_reports_dirs();
        assert!(
            dirs.iter()
                .any(|d| d == std::path::Path::new("/Library/Logs/DiagnosticReports")),
            "dirs: {dirs:?}"
        );
    }

    #[test]
    fn jetsam_finding_is_a_critical_root_cause() {
        let j = JetsamJoin {
            path: "/x/JetsamEvent.ips".into(),
            killed_pid: 4242,
            killed_name: "python".into(),
            footprint_bytes: 21_474_836_480,
            lifetime_max_bytes: 21_474_836_480,
            reason: Some("per-process-limit".into()),
        };
        let f = jetsam_finding(&j);
        assert_eq!(f.severity, Severity::Critical);
        assert_eq!(f.category, Category::RootCause);
        assert!(f.detail.contains("21"), "detail: {}", f.detail);
        assert!(f.detail.contains("jetsam") || f.title.contains("jetsam"));
        // The WHY of the kill must be surfaced: it is the question this
        // feature exists to answer.
        assert!(
            f.detail.contains("per-process-limit"),
            "detail: {}",
            f.detail
        );
    }

    /// With no kernel-reported reason the finding stays readable: no
    /// "Reason: None" and no truncated sentence.
    #[test]
    fn jetsam_finding_without_a_reason_stays_clean() {
        let j = JetsamJoin {
            path: "/x/JetsamEvent.ips".into(),
            killed_pid: 4242,
            killed_name: "python".into(),
            footprint_bytes: 21_474_836_480,
            lifetime_max_bytes: 21_474_836_480,
            reason: None,
        };
        let f = jetsam_finding(&j);
        assert!(!f.detail.contains("Reason"), "detail: {}", f.detail);
        assert!(!f.detail.contains("None"), "detail: {}", f.detail);
    }

    #[test]
    #[serial_test::serial]
    fn join_jetsam_inserts_the_finding_for_a_scoped_session() {
        use smeltr_core::session::{SessionId, SessionKind, SessionMetadata};
        use smeltr_core::writer::SessionWriter;

        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        // The session starts before the jetsam report is written, as in the
        // real chronology (#153): the kill, then the report, both land after
        // the session began.
        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.kind = SessionKind::Scoped {
            pid: 4242,
            argv: vec![],
        };
        let dir = {
            let w = SessionWriter::create(meta).unwrap();
            let d = w.dir().to_path_buf();
            drop(w);
            d
        };

        let f = reports.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();

        let mut report = crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        };
        join_jetsam(&mut report, &dir);

        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");
        assert_eq!(report.findings.len(), 1, "findings: {:#?}", report.findings);
        assert_eq!(report.findings[0].category, Category::RootCause);
    }

    /// Builds a scoped session on disk with controlled metadata (start, end,
    /// argv) and event stream, which no existing helper allows —
    /// `SessionWriter::create` always dates the start to now.
    fn scoped_session(
        home: &Path,
        pid: u32,
        argv: Vec<String>,
        started: &str,
        ended: Option<&str>,
        events: &[smeltr_core::event::Event],
    ) -> std::path::PathBuf {
        use smeltr_core::session::{SessionId, SessionKind, SessionMetadata};
        use smeltr_core::writer::SessionWriter;

        // SMELTR_HOME is already set by the caller (a #[serial] test).
        let _ = home;
        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.kind = SessionKind::Scoped { pid, argv };
        let mut w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        for e in events {
            w.write_event(e).unwrap();
        }
        w.flush().unwrap();
        drop(w);

        // Rewrite the timestamps AFTERWARDS: the writer forces "now".
        let mut meta = smeltr_core::reader::read_metadata(&dir).unwrap();
        meta.started_rfc3339 = started.to_string();
        meta.ended_rfc3339 = ended.map(str::to_string);
        smeltr_core::session::write_metadata(&dir, &meta).unwrap();
        dir
    }

    fn footprint_ev(
        seq: u64,
        ts_wall_ns: u64,
        pid: u32,
        name: &str,
        is_traced_root: bool,
    ) -> smeltr_core::event::Event {
        smeltr_core::event::Event {
            ts_mono_ns: seq,
            ts_wall_ns,
            session_id: uuid::Uuid::nil(),
            source: smeltr_core::event::Source::Proc,
            pid: Some(pid),
            seq,
            payload: Payload::ProcFootprint {
                pid,
                name: name.into(),
                phys_footprint_bytes: 1_000_000,
                lifetime_max_phys_footprint_bytes: 1_000_000,
                is_traced_root,
            },
        }
    }

    fn now_ns() -> u64 {
        time::OffsetDateTime::now_utc().unix_timestamp_nanos() as u64
    }

    /// The unbounded edge: a session that was never finalized — exactly what
    /// jetsam produces — used to have its window run all the way to NOW. A May
    /// session analyzed today would thus swallow every jetsam report of the
    /// following weeks, on the sole strength of a PID that macOS recycles.
    #[test]
    #[serial_test::serial]
    fn unfinalized_session_does_not_swallow_a_much_later_report() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        // Last event an hour ago; the session was never finalized (no
        // `ended_rfc3339`), as after a jetsam kill.
        let hour_ago = now_ns().saturating_sub(3_600_000_000_000);
        let dir = scoped_session(
            home.path(),
            4242,
            vec!["python".into()],
            "2026-05-15T17:35:05Z",
            None,
            &[footprint_ev(1, hour_ago, 4242, "python", true)],
        );

        // The report itself is written NOW: well after the session really
        // ended.
        let f = reports.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();

        let mut report = crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        };
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(
            report.findings.is_empty(),
            "unbounded window: {:#?}",
            report.findings
        );
    }

    /// The bounded fallback stays useful: a report written right after the last
    /// event of an unfinalized session must still be joined.
    #[test]
    #[serial_test::serial]
    fn unfinalized_session_still_joins_a_report_written_right_after() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let dir = scoped_session(
            home.path(),
            4242,
            vec!["python".into()],
            "2026-05-15T17:35:05Z",
            None,
            &[footprint_ev(1, now_ns(), 4242, "python", true)],
        );
        let f = reports.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();

        let mut report = crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        };
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
    }

    /// The PID alone is not enough: macOS recycles them. When both sides give
    /// a name and the names diverge, no join.
    #[test]
    #[serial_test::serial]
    fn name_mismatch_rejects_the_join() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        // The report names "python"; this session is a ruby run that simply
        // reused PID 4242.
        let dir = scoped_session(
            home.path(),
            4242,
            vec!["/usr/bin/ruby".into()],
            "2026-05-15T17:35:05Z",
            None,
            &[footprint_ev(1, now_ns(), 4242, "ruby", true)],
        );
        let f = reports.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();

        let mut report = crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        };
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(
            report.findings.is_empty(),
            "incompatible name joined anyway: {:#?}",
            report.findings
        );
    }

    /// The two sides truncate differently: `pbi_comm` is 16 bytes (MAXCOMLEN),
    /// a jetsam report's `name` runs to ~32 (observed on this machine:
    /// "com.apple.Virtualization.Virtual", 32 characters). Comparing for
    /// equality would reject the real case — we compare by prefix.
    #[test]
    fn truncated_names_match_by_prefix() {
        assert!(names_compatible(
            "com.apple.Virtua",
            "com.apple.Virtualization.Virtual"
        ));
        assert!(names_compatible("python", "python"));
        assert!(!names_compatible("ruby", "python"));
        // An empty name carries no information: it proves nothing.
        assert!(!names_compatible("", "python"));
    }

    /// With no name on one side we do not reject: PID + window remain the
    /// guards, as before. Rejecting would miss the very kill this feature
    /// exists to catch.
    #[test]
    #[serial_test::serial]
    fn missing_name_falls_back_to_pid_and_window() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        // Neither argv nor ProcFootprint: no name on the session side.
        let dir = scoped_session(
            home.path(),
            4242,
            vec![],
            "2026-05-15T17:35:05Z",
            Some(
                &time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap(),
            ),
            &[],
        );
        let f = reports.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();

        let mut report = crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        };
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
    }

    /// This project's normal case, not an edge case: under `uv run` /
    /// `poetry run` / `python -m`, the process that dies is a grandchild whose
    /// PID differs from the spawned child's — that is what `SMELTR_SCOPE_TOKEN`
    /// exists for (#31). The jetsam report names the grandchild; looking only
    /// at the scoped PID produced silence in exactly the targeted case.
    #[test]
    #[serial_test::serial]
    fn joins_a_grandchild_pid_seen_in_footprint_samples() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        // The session is scoped on the launcher (`uv`, PID 9999); the real
        // work — and the kill — is on the python grandchild 4242.
        let dir = scoped_session(
            home.path(),
            9999,
            vec!["/opt/homebrew/bin/uv".into()],
            "2026-05-15T17:35:05Z",
            None,
            &[
                footprint_ev(1, now_ns(), 9999, "uv", true),
                footprint_ev(2, now_ns(), 4242, "python", false),
            ],
        );
        let f = reports.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();

        let mut report = crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        };
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
        assert!(
            report.findings[0].detail.contains("4242"),
            "detail: {}",
            report.findings[0].detail
        );
    }

    /// Widening to observed PIDs must not open the floodgates: a PID the
    /// session never saw still gets no join.
    #[test]
    #[serial_test::serial]
    fn an_unobserved_pid_still_does_not_join() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let dir = scoped_session(
            home.path(),
            9999,
            vec!["/opt/homebrew/bin/uv".into()],
            "2026-05-15T17:35:05Z",
            None,
            &[footprint_ev(1, now_ns(), 9999, "uv", true)],
        );
        let f = reports.path().join("JetsamEvent-2026-08-09-123716.ips");
        std::fs::write(&f, JETSAM).unwrap();

        let mut report = crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        };
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(report.findings.is_empty(), "{:#?}", report.findings);
    }

    #[test]
    fn crash_finding_is_critical_root_cause() {
        let j = CrashJoin {
            path: "/x/Python.ips".into(),
            crashed_pid: 11672,
            signal: Some("SIGABRT".into()),
            summary: "EXC_CRASH".into(),
            exception_codes: vec!["0x0".into()],
        };
        let f = crash_finding(&j);
        assert_eq!(f.severity, Severity::Critical);
        assert_eq!(f.category, Category::RootCause);
        assert!(f.title.contains("SIGABRT"));
        assert!(f.detail.contains("/x/Python.ips"));
    }

    // ---- memory-death presumption (#201) ----

    /// Variant of `footprint_ev` with a chosen footprint.
    fn footprint_bytes_ev(
        seq: u64,
        ts_wall_ns: u64,
        pid: u32,
        bytes: u64,
    ) -> smeltr_core::event::Event {
        let mut e = footprint_ev(seq, ts_wall_ns, pid, "python3", true);
        if let Payload::ProcFootprint {
            phys_footprint_bytes,
            lifetime_max_phys_footprint_bytes,
            ..
        } = &mut e.payload
        {
            *phys_footprint_bytes = bytes;
            *lifetime_max_phys_footprint_bytes = bytes;
        }
        e
    }

    fn set_exit_code(dir: &Path, code: Option<i32>) {
        let mut meta = smeltr_core::reader::read_metadata(dir).unwrap();
        meta.exit_code = code;
        smeltr_core::session::write_metadata(dir, &meta).unwrap();
    }

    fn set_term_signal(dir: &Path, sig: Option<i32>) {
        let mut meta = smeltr_core::reader::read_metadata(dir).unwrap();
        meta.term_signal = sig;
        smeltr_core::session::write_metadata(dir, &meta).unwrap();
    }

    /// Like `risk_case`, but specifying the signal that killed the child.
    fn risk_case_signal(home: &Path, sig: Option<i32>) -> Vec<Finding> {
        let t = now_ns();
        let evs = vec![
            footprint_bytes_ev(1, t, 4242, 4_000_000_000),
            footprint_bytes_ev(2, t + 1_000_000_000, 4242, 18_000_000_000),
        ];
        let dir = scoped_session(
            home,
            4242,
            vec!["/usr/bin/python3".into()],
            "2026-08-09T10:00:00Z",
            Some("2026-08-09T10:05:00Z"),
            &evs,
        );
        set_exit_code(&dir, Some(-1));
        set_term_signal(&dir, sig);
        let mut report = empty_report();
        join_jetsam(&mut report, &dir);
        report.findings
    }

    fn empty_report() -> crate::report::Report {
        crate::report::Report {
            findings: Vec::new(),
            session_short: None,
            event_count: 0,
        }
    }

    /// Prepares a scoped session with chosen footprint, ending and exit code,
    /// then joins. Returns the findings produced.
    fn risk_case(
        home: &Path,
        first: u64,
        last: u64,
        ended: Option<&str>,
        exit_code: Option<i32>,
    ) -> Vec<Finding> {
        let t = now_ns();
        let evs = vec![
            footprint_bytes_ev(1, t, 4242, first),
            footprint_bytes_ev(2, t + 1_000_000_000, 4242, last),
        ];
        let dir = scoped_session(
            home,
            4242,
            vec!["/usr/bin/python3".into()],
            "2026-08-09T10:00:00Z",
            ended,
            &evs,
        );
        set_exit_code(&dir, exit_code);
        let mut report = empty_report();
        join_jetsam(&mut report, &dir);
        report.findings
    }

    /// THE case #201 reopens: the child is killed by a signal, no jetsam
    /// report is joinable, and the footprint was rising. The presumption fires.
    #[test]
    #[serial_test::serial]
    fn killed_by_signal_with_rising_footprint_yields_the_presumption() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let f = risk_case(home.path(), 4_000_000_000, 18_000_000_000, None, Some(-1));
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(f.len(), 1, "findings: {f:#?}");
        assert_eq!(f[0].severity, Severity::Warning);
        assert_eq!(f[0].category, Category::ContributingFactor);
        assert!(
            f[0].detail.contains("possible") || f[0].detail.contains("presumption"),
            "the wording must stay a presumption: {}",
            f[0].detail
        );
    }

    /// The original defect: a HEALTHY run that allocates heavily and ends
    /// cleanly must produce nothing. That is what the rule's first version
    /// shouted about on every run.
    #[test]
    #[serial_test::serial]
    fn clean_exit_with_rising_footprint_yields_nothing() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let f = risk_case(
            home.path(),
            15_000_000,
            1_007_000_000,
            Some("2026-08-09T10:05:00Z"),
            Some(0),
        );
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(f.is_empty(), "un run sain ne doit rien produire : {f:#?}");
    }

    /// A program failing normally (exit 1) is not a memory death.
    #[test]
    #[serial_test::serial]
    fn ordinary_failure_exit_yields_nothing() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let f = risk_case(
            home.path(),
            4_000_000_000,
            18_000_000_000,
            Some("2026-08-09T10:05:00Z"),
            Some(1),
        );
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(f.is_empty(), "{f:#?}");
    }

    /// Flat footprint plus a signal death: nothing to presume about memory.
    #[test]
    #[serial_test::serial]
    fn killed_by_signal_without_rising_footprint_yields_nothing() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let f = risk_case(home.path(), 4_000_000_000, 4_100_000_000, None, Some(-1));
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(f.is_empty(), "{f:#?}");
    }

    /// No exit code but WITH a dated ending: the daemon finalized on client
    /// disconnect, or boot recovery caught an orphaned session. Abnormal
    /// ending, so the presumption fires.
    #[test]
    #[serial_test::serial]
    fn finalized_without_exit_code_counts_as_abnormal() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let f = risk_case(
            home.path(),
            4_000_000_000,
            18_000_000_000,
            Some("2026-08-09T10:05:00Z"),
            None,
        );
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(f.len(), 1, "{f:#?}");
        assert_eq!(f[0].severity, Severity::Warning);
    }

    /// #204: the crash join must cover a session that was NEVER finalized. The
    /// old block in `analyze.rs` required `ended_rfc3339` and therefore
    /// silently skipped this case, in the CLI too — while that is exactly the
    /// shape a violent death produces.
    #[test]
    #[serial_test::serial]
    fn join_crash_covers_a_never_finalized_session() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        // An event stamped now: it is what bounds the window in the absence of
        // `ended_rfc3339`.
        let evs = vec![footprint_ev(1, now_ns(), 11672, "python3", true)];
        let dir = scoped_session(
            home.path(),
            11672,
            vec!["/usr/bin/python3".into()],
            "2026-08-09T10:00:00Z",
            None,
            &evs,
        );
        set_exit_code(&dir, Some(-1));
        std::fs::write(reports.path().join("Python-crash.ips"), MULTILINE).unwrap();

        let mut report = empty_report();
        join_crash(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
        assert_eq!(report.findings[0].category, Category::RootCause);
        assert!(report.findings[0].title.contains("crashed"));
    }

    /// A clean exit is not a crash: guard carried over from `analyze.rs`.
    #[test]
    #[serial_test::serial]
    fn join_crash_skips_a_clean_exit() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let evs = vec![footprint_ev(1, now_ns(), 11672, "python3", true)];
        let dir = scoped_session(
            home.path(),
            11672,
            vec!["/usr/bin/python3".into()],
            "2026-08-09T10:00:00Z",
            Some("2026-08-09T10:05:00Z"),
            &evs,
        );
        set_exit_code(&dir, Some(0));
        std::fs::write(reports.path().join("Python-crash.ips"), MULTILINE).unwrap();

        let mut report = empty_report();
        join_crash(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(report.findings.is_empty(), "{:#?}", report.findings);
    }

    /// A crash outranks the memory presumption: `join_crash` sets the root
    /// cause, which `presume_memory_death` then sees and honors. This is the
    /// scenario the MCP path misattributed before #204.
    #[test]
    #[serial_test::serial]
    fn a_crash_suppresses_the_memory_presumption() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let t = now_ns();
        let evs = vec![
            footprint_bytes_ev(1, t, 11672, 4_000_000_000),
            footprint_bytes_ev(2, t + 1_000_000_000, 11672, 18_000_000_000),
        ];
        // `ended` absent: the window bounds on the last event, stamped now, so
        // it covers the mtime of the report written just below. An end date
        // frozen in the past would put it out of window and the test would be
        // testing something else.
        let dir = scoped_session(
            home.path(),
            11672,
            vec!["/usr/bin/python3".into()],
            "2026-08-09T10:00:00Z",
            None,
            &evs,
        );
        set_exit_code(&dir, Some(-1));
        std::fs::write(reports.path().join("Python-crash.ips"), MULTILINE).unwrap();

        let mut report = empty_report();
        join_crash(&mut report, &dir);
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(
            report.findings.len(),
            1,
            "the crash must win, with no presumption added: {:#?}",
            report.findings
        );
        assert_eq!(report.findings[0].category, Category::RootCause);
    }

    /// #203: a Ctrl-C is a requested stop, not a memory death. It is the normal
    /// gesture for interrupting an inference that drags on, and a long run
    /// always clears the growth factor — without this guard, deliberately
    /// stopping a run would produce a memory-pressure presumption.
    #[test]
    #[serial_test::serial]
    fn sigint_is_a_requested_stop_not_a_memory_death() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let f = risk_case_signal(home.path(), Some(2)); // SIGINT
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(f.is_empty(), "a Ctrl-C must presume nothing: {f:#?}");
    }

    /// SIGTERM has the same nature: a stop requested by a supervisor.
    #[test]
    #[serial_test::serial]
    fn sigterm_is_a_requested_stop_too() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let f = risk_case_signal(home.path(), Some(15)); // SIGTERM
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(f.is_empty(), "{f:#?}");
    }

    /// SIGKILL remains a death suffered: that is what jetsam does, so the
    /// presumption must fire. This is the other half of the discrimination.
    #[test]
    #[serial_test::serial]
    fn sigkill_still_yields_the_presumption() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let f = risk_case_signal(home.path(), Some(9)); // SIGKILL
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(f.len(), 1, "{f:#?}");
        assert_eq!(f[0].severity, Severity::Warning);
    }

    /// Sessions predating #203: `term_signal` is absent. We must not lose the
    /// signal for that — the behavior falls back to `exit_code`.
    #[test]
    #[serial_test::serial]
    fn a_session_without_a_recorded_signal_still_works() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let f = risk_case_signal(home.path(), None);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(f.len(), 1, "{f:#?}");
    }

    /// The false positive to keep out at all costs: a session STILL RUNNING has
    /// neither exit code nor dated ending, and its footprint necessarily
    /// rises. A `smeltr analyze --last` during a long run must not announce an
    /// abnormal ending on a session that is doing fine.
    #[test]
    #[serial_test::serial]
    fn live_in_progress_session_yields_nothing() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        // Neither `ended` nor `exit_code`: the exact shape of a live session.
        let f = risk_case(home.path(), 4_000_000_000, 18_000_000_000, None, None);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert!(
            f.is_empty(),
            "a running session has not ended abnormally: {f:#?}"
        );
    }

    /// When a root cause is already established (a crash joined upstream), the
    /// memory presumption stays quiet: a verdict outranks a guess.
    #[test]
    #[serial_test::serial]
    fn existing_root_cause_suppresses_the_presumption() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let reports = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_DIAGNOSTIC_REPORTS_DIR", reports.path());

        let t = now_ns();
        let evs = vec![
            footprint_bytes_ev(1, t, 4242, 4_000_000_000),
            footprint_bytes_ev(2, t + 1_000_000_000, 4242, 18_000_000_000),
        ];
        let dir = scoped_session(
            home.path(),
            4242,
            vec!["/usr/bin/python3".into()],
            "2026-08-09T10:00:00Z",
            None,
            &evs,
        );
        set_exit_code(&dir, Some(-1));

        let mut report = empty_report();
        report.findings.push(Finding::new(
            Severity::Critical,
            Category::RootCause,
            "Recorded process crashed (SIGSEGV)",
        ));
        join_jetsam(&mut report, &dir);
        std::env::remove_var("SMELTR_DIAGNOSTIC_REPORTS_DIR");

        assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
        assert_eq!(report.findings[0].category, Category::RootCause);
    }
}

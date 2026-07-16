//! Analyze-time crash-report join (#153).
//!
//! ReportCrash writes the `.ips` seconds AFTER the crashed process dies —
//! by then the scoped session is already finalized (the record client's
//! connection dropped, #143), so the live crash-reports probe cannot land
//! the report in the crashed session. This module joins retroactively:
//! given the crashed session's child pid and wall-clock window, it scans
//! the DiagnosticReports directory for a matching report and turns it
//! into a RootCause finding. Works on sessions recorded before the fix.

use crate::finding::{Category, Finding, Severity};
use smeltr_core::event::Payload;
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
}

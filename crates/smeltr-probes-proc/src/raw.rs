#[derive(Debug, Clone, PartialEq)]
pub struct ProcSample {
    pub pid: u32,
    pub name: String,
    pub cpu_pct: f32,
}

pub const FLAGGED_NAMES: &[&str] = &[
    "ReportCrash",
    "diagnosticservicesd",
    "UserNotificationCenter",
    "spindump",
];

pub const DEFAULT_FLAG_CPU_PCT: f32 = 5.0;

pub fn top_and_flagged(
    mut samples: Vec<ProcSample>,
    n: usize,
    flag_threshold_pct: f32,
) -> (Vec<ProcSample>, Vec<String>) {
    let flagged: Vec<String> = samples
        .iter()
        .filter(|s| FLAGGED_NAMES.contains(&s.name.as_str()) && s.cpu_pct > flag_threshold_pct)
        .map(|s| s.name.clone())
        .collect();
    samples.sort_by(|a, b| {
        b.cpu_pct
            .partial_cmp(&a.cpu_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    samples.truncate(n);
    (samples, flagged)
}

/// System-wide per-process CPU snapshot.
///
/// `ps`, not `top -l 1`. The previous implementation forked
/// `top -l 1 -n 50 -stats pid,command,cpu`, which returns **0.0 for every
/// process**: the CPU column is a difference between two samples and the
/// first sample has nothing to difference against. Measured on a process
/// burning a full core -- `ps` 100.0, `top -l 1` 0.0, `top -l 2 -s 1` 78.3 --
/// and confirmed on recorded sessions, where all 18276 `ProcTop` events
/// carried `cpu_pct: 0` (#217).
///
/// `ps -c` also leaves the process name intact, where `top` truncated
/// COMMAND to 16 characters -- long enough to break `diagnosticservicesd`
/// and `UserNotificationCenter`, two of the four `FLAGGED_NAMES`.
///
/// Still a fork, deliberately. Syscall enumeration cannot replace it: both
/// `proc_pid_rusage` and `proc_pidinfo(PROC_PIDTBSDINFO)` return EPERM for
/// root-owned processes (verified on pid 1, `logd`, `syslogd`), and
/// `proc_listallpids` saw 285 of this machine's 467 processes. The daemons
/// worth flagging -- `diagnosticservicesd`, `spindump` -- are exactly the
/// ones that would disappear. `ps` costs 0.02 s against `top`'s 0.43 s, so
/// the fork is 20x cheaper than the one it replaces.
///
/// `%CPU` here is the kernel's decaying average over roughly the last
/// minute, not an instantaneous reading. That suits `system_pressure`,
/// which looks for sustained conditions rather than momentary spikes.
#[cfg(target_os = "macos")]
pub fn read_sys() -> std::io::Result<Vec<ProcSample>> {
    use std::process::Command;
    // -c: executable name only, untruncated. COMM last so a name containing
    // spaces cannot be confused with the following column.
    // LC_ALL=C: a decimal-comma locale prints `%cpu` as `0,1`, and every
    // row then failed to parse — an empty ProcTop, silently (#244).
    let out = Command::new("/bin/ps")
        .args(["-axco", "pid,%cpu,comm"])
        .env("LC_ALL", "C")
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!("ps exited {:?}", out.status)));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(parse_ps(&stdout))
}

#[cfg(not(target_os = "macos"))]
pub fn read_sys() -> std::io::Result<Vec<ProcSample>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "proc probe requires macOS",
    ))
}

/// Parses `ps -axco pid,%cpu,comm` output: `<pid> <cpu> <name...>`, one
/// process per line after the header. Unparseable lines are skipped rather
/// than failing the tick.
pub fn parse_ps(stdout: &str) -> Vec<ProcSample> {
    let mut samples = Vec::new();
    for line in stdout.lines().skip(1) {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let Ok(pid) = parts[0].parse::<u32>() else {
            continue;
        };
        let Ok(cpu) = parts[1].parse::<f32>() else {
            continue;
        };
        samples.push(ProcSample {
            pid,
            name: parts[2..].join(" "),
            cpu_pct: cpu,
        });
    }
    samples
}

#[cfg(all(test, target_os = "macos"))]
mod locale_tests {
    use super::*;

    /// #244: under a French locale `ps` prints `%cpu` as `0,1`, every row
    /// failed to parse, and `ProcTop` came back empty with no error.
    #[test]
    #[serial_test::serial]
    fn read_sys_survives_a_decimal_comma_locale() {
        let before = std::env::var_os("LC_ALL");
        std::env::set_var("LC_ALL", "fr_FR.UTF-8");
        let rows = read_sys();
        match before {
            Some(v) => std::env::set_var("LC_ALL", v),
            None => std::env::remove_var("LC_ALL"),
        }
        assert!(!rows.unwrap().is_empty(), "no process parsed");
    }
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    const HEADER: &str = "  PID  %CPU COMM\n";

    #[test]
    fn parse_ps_extracts_rows() {
        let sample = format!(
            "{HEADER}\
                 1   0.0 launchd\n\
              1234  17.8 ReportCrash\n\
              5678   4.2 python3.11\n"
        );
        let r = parse_ps(&sample);
        assert_eq!(r.len(), 3);
        assert_eq!(r[1].pid, 1234);
        assert_eq!(r[1].name, "ReportCrash");
        assert!((r[1].cpu_pct - 17.8).abs() < 0.01);
    }

    #[test]
    fn parse_ps_keeps_long_and_spaced_names() {
        // `top -l 1` truncated COMMAND to 16 characters, so FLAGGED_NAMES
        // entries longer than that could never match (#217). `ps -c` puts the
        // name last and does not truncate it.
        let sample = format!(
            "{HEADER}\
               100   9.0 diagnosticservicesd\n\
               101   9.0 UserNotificationCenter\n\
               102   1.0 Core Audio Driver (ParrotAudioPlugin.driver)\n"
        );
        let r = parse_ps(&sample);
        assert_eq!(r[0].name, "diagnosticservicesd");
        assert_eq!(r[1].name, "UserNotificationCenter");
        assert_eq!(r[2].name, "Core Audio Driver (ParrotAudioPlugin.driver)");
    }

    #[test]
    fn parse_ps_skips_header_and_malformed_lines() {
        let sample = format!("{HEADER}not a row\n  12\n  13   x.y name\n  14   1.0 ok\n");
        let r = parse_ps(&sample);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].pid, 14);
    }

    /// Regression for #217: the probe reported `cpu_pct: 0` for every process
    /// on every tick, because `top -l 1` has no previous sample to difference
    /// against. 18276 ProcTop events in one real session, all zero, so
    /// `system_pressure` could never fire and the TUI panel showed only zeros.
    #[cfg(target_os = "macos")]
    #[test]
    fn read_sys_reports_nonzero_cpu_for_a_busy_process() {
        // Give the machine something unambiguous to report. `%CPU` is a
        // decaying average, so the burn has to precede the read.
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s2 = stop.clone();
        let h = std::thread::spawn(move || {
            while !s2.load(std::sync::atomic::Ordering::Relaxed) {
                std::hint::black_box((0..10_000).sum::<u64>());
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(1500));
        let samples = read_sys().expect("read_sys failed");
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = h.join();

        assert!(!samples.is_empty(), "no processes enumerated");
        let busiest = samples.iter().map(|s| s.cpu_pct).fold(0.0_f32, f32::max);
        assert!(
            busiest > 0.0,
            "every process reported 0% CPU -- this is the #217 defect, not an idle machine"
        );
    }

    /// The process names must survive the round trip too: a run where every
    /// name is 16 characters is the `top` truncation coming back.
    #[cfg(target_os = "macos")]
    #[test]
    fn read_sys_returns_untruncated_names() {
        let samples = read_sys().expect("read_sys failed");
        assert!(!samples.is_empty());
        assert!(
            samples.iter().all(|s| !s.name.is_empty()),
            "empty process name in {samples:?}"
        );
        let longest = samples.iter().map(|s| s.name.len()).max().unwrap_or(0);
        assert!(
            longest > 16,
            "no name longer than 16 chars: names look truncated ({longest})"
        );
    }
}

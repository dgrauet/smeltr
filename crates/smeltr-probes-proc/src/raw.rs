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

#[cfg(target_os = "macos")]
pub fn read_sys() -> std::io::Result<Vec<ProcSample>> {
    use std::process::Command;
    let out = Command::new("/usr/bin/top")
        .args(["-l", "1", "-n", "50", "-stats", "pid,command,cpu"])
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(format!(
            "top exited {:?}",
            out.status
        )));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(parse_top(&stdout))
}

#[cfg(not(target_os = "macos"))]
pub fn read_sys() -> std::io::Result<Vec<ProcSample>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "proc probe requires macOS",
    ))
}

pub fn parse_top(stdout: &str) -> Vec<ProcSample> {
    let mut samples = Vec::new();
    let mut in_data = false;
    for line in stdout.lines() {
        let line = line.trim();
        if line.starts_with("PID") && line.contains("%CPU") {
            in_data = true;
            continue;
        }
        if !in_data || line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let pid: u32 = match parts[0].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let cpu: f32 = match parts.last().unwrap().parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let name = parts[1..parts.len() - 1].join(" ");
        samples.push(ProcSample {
            pid,
            name,
            cpu_pct: cpu,
        });
    }
    samples
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    #[test]
    fn parse_top_extracts_rows() {
        let sample = "Processes: 100\n\
                      PID    COMMAND          %CPU\n\
                      1      launchd          0.0\n\
                      1234   ReportCrash      17.8\n\
                      5678   python           4.2\n";
        let r = parse_top(sample);
        assert_eq!(r.len(), 3);
        assert_eq!(r[1].pid, 1234);
        assert_eq!(r[1].name, "ReportCrash");
        assert!((r[1].cpu_pct - 17.8).abs() < 0.01);
    }
}

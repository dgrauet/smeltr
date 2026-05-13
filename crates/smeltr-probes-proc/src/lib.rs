pub mod probe;
pub mod raw;

pub use probe::ProcProbe;
pub use raw::{read_sys, top_and_flagged, ProcSample, DEFAULT_FLAG_CPU_PCT, FLAGGED_NAMES};

#[cfg(test)]
mod tests {
    use super::*;

    fn s(pid: u32, name: &str, cpu: f32) -> ProcSample {
        ProcSample {
            pid,
            name: name.into(),
            cpu_pct: cpu,
        }
    }

    #[test]
    fn top_n_sorts_by_cpu_desc() {
        let samples = vec![s(1, "a", 1.0), s(2, "b", 3.0), s(3, "c", 2.0)];
        let (top, _flagged) = top_and_flagged(samples, 2, DEFAULT_FLAG_CPU_PCT);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].pid, 2);
        assert_eq!(top[1].pid, 3);
    }

    #[test]
    fn flagged_includes_reportcrash_above_threshold() {
        let samples = vec![s(1, "ReportCrash", 17.8), s(2, "WindowServer", 5.0)];
        let (_top, flagged) = top_and_flagged(samples, 5, 5.0);
        assert!(flagged.contains(&"ReportCrash".to_string()));
        assert!(!flagged.contains(&"WindowServer".to_string()));
    }

    #[test]
    fn flagged_ignores_named_process_below_threshold() {
        let samples = vec![s(1, "ReportCrash", 2.0)];
        let (_top, flagged) = top_and_flagged(samples, 5, 5.0);
        assert!(flagged.is_empty());
    }

    #[test]
    fn flagged_recognizes_all_known_names() {
        for name in FLAGGED_NAMES {
            let samples = vec![s(1, name, 50.0)];
            let (_top, flagged) = top_and_flagged(samples, 5, 5.0);
            assert!(flagged.contains(&name.to_string()), "missed: {name}");
        }
    }
}

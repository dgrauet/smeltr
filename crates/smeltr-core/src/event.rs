//! Event model. All producers emit values of this type.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    Mark,
    System,
    IoReport,
    Vm,
    Proc,
    OsLog,
    Thermal,
    MachExc,
    CrashReport,
    MetalHook,
    PythonSidecar,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcEntry {
    pub pid: u32,
    pub name: String,
    pub cpu_pct: f32,
}

impl Eq for ProcEntry {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProbeHealthState {
    Ok,
    Degraded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Payload {
    Mark {
        label: String,
    },
    SessionStarted {
        wall_unix_ns: u64,
    },
    SessionEnded {
        wall_unix_ns: u64,
        reason: String,
    },
    VmSample {
        wired_bytes: u64,
        active_bytes: u64,
        compressed_bytes: u64,
        swap_used_bytes: u64,
        page_outs_per_sec: f32,
    },
    ProcTop {
        top: Vec<ProcEntry>,
        flagged: Vec<String>,
    },
    ThermalState {
        level: u32,
    },
    IoReportSample {
        gpu_residency_pct: Option<f32>,
        ane_residency_pct: Option<f32>,
        cpu_residency_pct: Option<f32>,
        gpu_power_mw: Option<u32>,
        gpu_freq_mhz: Option<u32>,
    },
    OsLogLine {
        ts_wall_ns: u64,
        subsystem: String,
        category: String,
        message: String,
    },
    MachException {
        target_pid: u32,
        exception_type: i32,
        codes: Vec<i64>,
    },
    CrashReportEmitted {
        path: String,
        crashed_pid: Option<u32>,
        signal: Option<String>,
        exception_codes: Vec<String>,
        summary: String,
    },
    ProbeHealth {
        probe: String,
        state: ProbeHealthState,
        reason: Option<String>,
    },
}

impl Eq for Payload {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub ts_mono_ns: u64,
    pub ts_wall_ns: u64,
    pub session_id: Uuid,
    pub source: Source,
    pub pid: Option<u32>,
    pub seq: u64,
    pub payload: Payload,
}

impl Eq for Event {}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(payload: Payload, source: Source) {
        let e = Event {
            ts_mono_ns: 1,
            ts_wall_ns: 2,
            session_id: Uuid::nil(),
            source,
            pid: Some(42),
            seq: 1,
            payload,
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&e, &mut buf).unwrap();
        let decoded: Event = ciborium::from_reader(&buf[..]).unwrap();
        assert_eq!(e, decoded);
    }

    #[test]
    fn cbor_round_trip_mark() {
        round_trip(Payload::Mark { label: "x".into() }, Source::Mark);
    }

    #[test]
    fn cbor_round_trip_session_started() {
        round_trip(Payload::SessionStarted { wall_unix_ns: 9 }, Source::System);
    }

    #[test]
    fn cbor_round_trip_session_ended() {
        round_trip(
            Payload::SessionEnded {
                wall_unix_ns: 9,
                reason: "r".into(),
            },
            Source::System,
        );
    }

    #[test]
    fn cbor_round_trip_vm_sample() {
        round_trip(
            Payload::VmSample {
                wired_bytes: 1,
                active_bytes: 2,
                compressed_bytes: 3,
                swap_used_bytes: 4,
                page_outs_per_sec: 5.0,
            },
            Source::Vm,
        );
    }

    #[test]
    fn cbor_round_trip_proc_top() {
        round_trip(
            Payload::ProcTop {
                top: vec![ProcEntry {
                    pid: 1,
                    name: "x".into(),
                    cpu_pct: 1.0,
                }],
                flagged: vec!["ReportCrash".into()],
            },
            Source::Proc,
        );
    }

    #[test]
    fn cbor_round_trip_thermal_state() {
        round_trip(Payload::ThermalState { level: 2 }, Source::Thermal);
    }

    #[test]
    fn cbor_round_trip_io_report_sample() {
        round_trip(
            Payload::IoReportSample {
                gpu_residency_pct: Some(80.0),
                ane_residency_pct: None,
                cpu_residency_pct: Some(50.0),
                gpu_power_mw: Some(3500),
                gpu_freq_mhz: Some(1400),
            },
            Source::IoReport,
        );
    }

    #[test]
    fn cbor_round_trip_os_log_line() {
        round_trip(
            Payload::OsLogLine {
                ts_wall_ns: 1,
                subsystem: "com.apple.gpurestart".into(),
                category: "default".into(),
                message: "GPU watchdog".into(),
            },
            Source::OsLog,
        );
    }

    #[test]
    fn cbor_round_trip_mach_exception() {
        round_trip(
            Payload::MachException {
                target_pid: 1234,
                exception_type: 10,
                codes: vec![1, 2],
            },
            Source::MachExc,
        );
    }

    #[test]
    fn cbor_round_trip_crash_report() {
        round_trip(
            Payload::CrashReportEmitted {
                path: "/x.ips".into(),
                crashed_pid: Some(1234),
                signal: Some("SIGABRT".into()),
                exception_codes: vec!["0x0e".into()],
                summary: "kIOGPU...".into(),
            },
            Source::CrashReport,
        );
    }

    #[test]
    fn cbor_round_trip_probe_health() {
        round_trip(
            Payload::ProbeHealth {
                probe: "vm".into(),
                state: ProbeHealthState::Degraded,
                reason: Some("no SMC keys".into()),
            },
            Source::System,
        );
    }
}

//! Bus subscriber that watches for crash-like events and flushes the flight
//! recorder into a standalone post-mortem session on disk.

use crate::flight_recorder::FlightRecorder;
use smeltr_core::event::{Event, Payload};
use smeltr_core::session::{sessions_root, SessionId, SessionMetadata};
use std::sync::Arc;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerReason {
    CrashReport { path: String },
    MachException { target_pid: u32 },
    MetalError { cb_id: u64, error_code: i64 },
}

impl TriggerReason {
    pub fn label(&self) -> String {
        match self {
            Self::CrashReport { .. } => "crash-report".into(),
            Self::MachException { .. } => "mach-exception".into(),
            Self::MetalError { error_code, .. } => format!("metal-error-{error_code}"),
        }
    }
}

/// Inspects a single event and returns a TriggerReason if it should fire.
pub fn classify(ev: &Event) -> Option<TriggerReason> {
    match &ev.payload {
        Payload::CrashReportEmitted { path, .. } => {
            Some(TriggerReason::CrashReport { path: path.clone() })
        }
        Payload::MachException { target_pid, .. } => Some(TriggerReason::MachException {
            target_pid: *target_pid,
        }),
        Payload::MetalCbCompleted {
            cb_id,
            error_code: Some(c),
            ..
        } if *c != 0 => Some(TriggerReason::MetalError {
            cb_id: *cb_id,
            error_code: *c,
        }),
        _ => None,
    }
}

pub struct FlushSummary {
    pub session_dir: std::path::PathBuf,
    pub event_count: usize,
}

/// Drains the flight recorder snapshot into a new on-disk session named
/// `post-mortem-<label>-<ts>-<short>`. Errors are returned, never panicked.
pub fn flush_post_mortem(
    fr: &Arc<FlightRecorder>,
    reason: &TriggerReason,
) -> std::io::Result<FlushSummary> {
    let events = fr.snapshot();
    if events.is_empty() {
        return Err(std::io::Error::other("flight recorder is empty"));
    }
    let id = SessionId::new();
    let meta = SessionMetadata::now_starting(id);
    let root = sessions_root();
    std::fs::create_dir_all(&root)?;
    let t = OffsetDateTime::parse(&meta.started_rfc3339, &Rfc3339)
        .unwrap_or_else(|_| OffsetDateTime::now_utc());
    let dir_name = format!(
        "post-mortem-{}-{:04}-{:02}-{:02}-{:02}{:02}{:02}-{}",
        reason.label(),
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        id.short(),
    );
    let dir = root.join(&dir_name);
    std::fs::create_dir_all(&dir)?;
    let events_path = dir.join("events.cbor.zst");
    let file = std::fs::File::create(&events_path)?;
    let mut enc = zstd::stream::Encoder::new(file, 3)?;
    for ev in &events {
        smeltr_core::codec::write_frame(&mut enc, ev)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
    }
    enc.finish()?;
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    let toml_str = format!(
        "session_id = \"{}\"\nstarted_rfc3339 = \"{}\"\nended_rfc3339 = \"{}\"\nhost = \"{}\"\nargv = [\"post-mortem:{}\"]\n",
        meta.session_id, meta.started_rfc3339, now, meta.host, reason.label(),
    );
    std::fs::write(dir.join("metadata.toml"), toml_str)?;
    Ok(FlushSummary {
        session_dir: dir,
        event_count: events.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::Source;
    use uuid::Uuid;

    fn wrap(payload: Payload) -> Event {
        Event {
            ts_mono_ns: 1,
            ts_wall_ns: 1,
            session_id: Uuid::nil(),
            source: Source::System,
            pid: None,
            seq: 1,
            payload,
        }
    }

    #[test]
    fn crash_report_fires() {
        let ev = wrap(Payload::CrashReportEmitted {
            path: "/tmp/foo.ips".into(),
            crashed_pid: Some(123),
            signal: Some("SIGSEGV".into()),
            exception_codes: vec![],
            summary: "boom".into(),
        });
        assert!(matches!(
            classify(&ev),
            Some(TriggerReason::CrashReport { .. })
        ));
    }

    #[test]
    fn mach_exception_fires() {
        let ev = wrap(Payload::MachException {
            target_pid: 42,
            exception_type: 0,
            codes: vec![],
        });
        assert!(matches!(
            classify(&ev),
            Some(TriggerReason::MachException { target_pid: 42 })
        ));
    }

    #[test]
    fn metal_completed_with_error_fires() {
        let ev = wrap(Payload::MetalCbCompleted {
            cb_id: 99,
            queue_id: 1,
            status: 4,
            error_code: Some(14),
            error_domain: Some("IOGPU".into()),
            in_flight_ns: 9_000_000_000,
        });
        assert!(matches!(
            classify(&ev),
            Some(TriggerReason::MetalError {
                cb_id: 99,
                error_code: 14
            })
        ));
    }

    #[test]
    fn metal_completed_with_zero_error_does_not_fire() {
        let ev = wrap(Payload::MetalCbCompleted {
            cb_id: 1,
            queue_id: 1,
            status: 4,
            error_code: Some(0),
            error_domain: None,
            in_flight_ns: 0,
        });
        assert!(classify(&ev).is_none());
    }

    #[test]
    fn ordinary_mark_does_not_fire() {
        let ev = wrap(Payload::Mark { label: "x".into() });
        assert!(classify(&ev).is_none());
    }

    #[test]
    #[serial_test::serial]
    fn flush_writes_session_dir_with_events() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());

        let fr = std::sync::Arc::new(crate::flight_recorder::FlightRecorder::new(
            std::time::Duration::from_secs(60),
        ));
        for i in 0..5 {
            fr.push(Event {
                ts_mono_ns: i * 100,
                ts_wall_ns: 0,
                session_id: uuid::Uuid::nil(),
                source: smeltr_core::event::Source::Mark,
                pid: None,
                seq: i,
                payload: Payload::Mark {
                    label: format!("e-{i}"),
                },
            });
        }
        let reason = TriggerReason::MetalError {
            cb_id: 1,
            error_code: 14,
        };
        let summary = flush_post_mortem(&fr, &reason).unwrap();
        assert_eq!(summary.event_count, 5);
        assert!(summary
            .session_dir
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("metal-error-14"));
        let evs = smeltr_core::reader::read_events(&summary.session_dir).unwrap();
        assert_eq!(evs.len(), 5);
    }
}

//! `query_events` tool: filter session events by source/payload-kind/time.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};
use smeltr_core::event::{Event, Source};

const DEFAULT_LIMIT: usize = 1000;

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct Params {
    pub session: String,
    pub source: Option<String>,
    pub payload_kind: Option<String>,
    pub from_ts_mono_ns: Option<u64>,
    pub to_ts_mono_ns: Option<u64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub events: Vec<Event>,
    pub matched: usize,
    pub total: usize,
    pub truncated: bool,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let dir = resolve_session(&params.session)?;
    let events = smeltr_core::reader::read_events(&dir)?;
    let total = events.len();
    let want_source = match params.source.as_deref() {
        None => None,
        Some(s) => Some(parse_source(s)?),
    };
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT);

    let mut filtered: Vec<Event> = events
        .into_iter()
        .filter(|e| match want_source {
            None => true,
            Some(s) => e.source == s,
        })
        .filter(|e| match (params.from_ts_mono_ns, params.to_ts_mono_ns) {
            (Some(from), _) if e.ts_mono_ns < from => false,
            (_, Some(to)) if e.ts_mono_ns > to => false,
            _ => true,
        })
        .filter(|e| match params.payload_kind.as_deref() {
            None => true,
            Some(want) => payload_kind(e).eq_ignore_ascii_case(want),
        })
        .collect();

    let matched = filtered.len();
    let truncated = matched > limit;
    filtered.truncate(limit);
    Ok(Response {
        events: filtered,
        matched,
        total,
        truncated,
    })
}

fn parse_source(s: &str) -> Result<Source, ToolError> {
    Ok(match s {
        "Mark" => Source::Mark,
        "System" => Source::System,
        "IoReport" => Source::IoReport,
        "Vm" => Source::Vm,
        "Proc" => Source::Proc,
        "OsLog" => Source::OsLog,
        "Thermal" => Source::Thermal,
        "MachExc" => Source::MachExc,
        "CrashReport" => Source::CrashReport,
        "MetalHook" => Source::MetalHook,
        "PythonSidecar" => Source::PythonSidecar,
        other => return Err(ToolError::BadArgs(format!("unknown source {other:?}"))),
    })
}

fn payload_kind(e: &Event) -> &'static str {
    use smeltr_core::event::Payload;
    match &e.payload {
        Payload::Mark { .. } => "Mark",
        Payload::SessionStarted { .. } => "SessionStarted",
        Payload::SessionEnded { .. } => "SessionEnded",
        Payload::VmSample { .. } => "VmSample",
        Payload::ProcTop { .. } => "ProcTop",
        Payload::ThermalState { .. } => "ThermalState",
        Payload::IoReportSample { .. } => "IoReportSample",
        Payload::OsLogLine { .. } => "OsLogLine",
        Payload::MachException { .. } => "MachException",
        Payload::CrashReportEmitted { .. } => "CrashReportEmitted",
        Payload::MetalCbCommitted { .. } => "MetalCbCommitted",
        Payload::MetalCbScheduled { .. } => "MetalCbScheduled",
        Payload::MetalCbCompleted { .. } => "MetalCbCompleted",
        Payload::MetalCbWarning { .. } => "MetalCbWarning",
        Payload::MetalHeapAlloc { .. } => "MetalHeapAlloc",
        Payload::MetalDeviceMemSample { .. } => "MetalDeviceMemSample",
        Payload::MetalHeapFree { .. } => "MetalHeapFree",
        Payload::MetalBufferAlloc { .. } => "MetalBufferAlloc",
        Payload::MetalBufferFree { .. } => "MetalBufferFree",
        Payload::MetalTextureAlloc { .. } => "MetalTextureAlloc",
        Payload::MetalTextureFree { .. } => "MetalTextureFree",
        Payload::MetalHookDropped { .. } => "MetalHookDropped",
        Payload::MetalHookSkipped { .. } => "MetalHookSkipped",
        Payload::MlxEvalEntered { .. } => "MlxEvalEntered",
        Payload::MlxEvalReturned { .. } => "MlxEvalReturned",
        Payload::MlxMemoryPoll { .. } => "MlxMemoryPoll",
        Payload::MlxArrayAlive { .. } => "MlxArrayAlive",
        Payload::MlxArrayFreed { .. } => "MlxArrayFreed",
        Payload::MlxSnapshot { .. } => "MlxSnapshot",
        Payload::MlxPanicTriggered { .. } => "MlxPanicTriggered",
        Payload::PythonSidecarHello { .. } => "PythonSidecarHello",
        Payload::PostMortemFlushed { .. } => "PostMortemFlushed",
        Payload::ProbeHealth { .. } => "ProbeHealth",
        Payload::ModuleEntered { .. } => "ModuleEntered",
        Payload::ModuleReturned { .. } => "ModuleReturned",
        Payload::MetalCbOps { .. } => "MetalCbOps",
        Payload::ModelLoad { .. } => "ModelLoad",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::Payload;
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;
    use uuid::Uuid;

    fn write_session() -> SessionId {
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let mut w = SessionWriter::create(meta).unwrap();
        for i in 0..5 {
            w.write_event(&Event {
                ts_mono_ns: i * 100,
                ts_wall_ns: 0,
                session_id: Uuid::nil(),
                source: Source::Mark,
                pid: None,
                seq: i,
                payload: Payload::Mark {
                    label: format!("m-{i}"),
                    fields: Default::default(),
                },
            })
            .unwrap();
        }
        w.write_event(&Event {
            ts_mono_ns: 1000,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq: 99,
            payload: Payload::MetalCbCommitted {
                cb_id: 1,
                queue_id: 1,
                queue_depth: 1,
                label: None,
            },
        })
        .unwrap();
        w.finalize(Some(0), "x".into()).unwrap();
        id
    }

    #[test]
    #[serial_test::serial]
    fn filter_by_source_returns_only_matching() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = write_session();
        let resp = run(Params {
            session: id.short(),
            source: Some("Mark".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resp.matched, 5);
        assert!(resp.events.iter().all(|e| e.source == Source::Mark));
    }

    #[test]
    #[serial_test::serial]
    fn filter_by_payload_kind() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = write_session();
        let resp = run(Params {
            session: id.short(),
            payload_kind: Some("MetalCbCommitted".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resp.matched, 1);
    }

    #[test]
    #[serial_test::serial]
    fn limit_truncates() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = write_session();
        let resp = run(Params {
            session: id.short(),
            limit: Some(2),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(resp.events.len(), 2);
        assert!(resp.matched > 2);
        assert!(resp.truncated);
    }
}

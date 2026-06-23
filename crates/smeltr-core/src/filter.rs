//! Shared event predicate used by the reader's filtered path and query_events.
use crate::chunked::ChunkIndexEntry;
use crate::event::{Event, Payload, Source};

#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub source: Option<Source>,
    pub from_ts: Option<u64>,
    pub to_ts: Option<u64>,
    pub payload_kind: Option<String>,
}

impl EventFilter {
    /// Returns `true` if the event satisfies all filter criteria.
    ///
    /// Time bounds are **inclusive**. `payload_kind` comparison is
    /// case-insensitive. An `EventFilter` with all fields set to `None`
    /// matches every event.
    pub fn matches(&self, e: &Event) -> bool {
        if let Some(s) = self.source {
            if e.source != s {
                return false;
            }
        }
        if let Some(from) = self.from_ts {
            if e.ts_mono_ns < from {
                return false;
            }
        }
        if let Some(to) = self.to_ts {
            if e.ts_mono_ns > to {
                return false;
            }
        }
        if let Some(k) = &self.payload_kind {
            if !payload_kind(e).eq_ignore_ascii_case(k) {
                return false;
            }
        }
        true
    }

    /// Returns `true` if the chunk *possibly* contains a matching event.
    ///
    /// Uses the chunk's `min_ts`/`max_ts` range and `source_bitmap` for
    /// quick elimination. A `false` return means no event in the chunk can
    /// satisfy the filter; a `true` return is not a guarantee (conservative).
    pub fn chunk_overlaps(&self, c: &ChunkIndexEntry) -> bool {
        if let Some(from) = self.from_ts {
            if c.max_ts < from {
                return false;
            }
        }
        if let Some(to) = self.to_ts {
            if c.min_ts > to {
                return false;
            }
        }
        if let Some(s) = self.source {
            if c.source_bitmap & (1u64 << s.as_u8()) == 0 {
                return false;
            }
        }
        true
    }
}

/// Canonical payload-kind string (moved from `query_events` so there is one site).
///
/// Returns a `'static str` that names the `Payload` variant.  Used for
/// case-insensitive filtering via `EventFilter::matches`.
pub fn payload_kind(e: &Event) -> &'static str {
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
        Payload::ModelUnload { .. } => "ModelUnload",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunked::ChunkIndexEntry;
    use crate::event::{Event, Payload, Source};
    use uuid::Uuid;

    fn ev(ts: u64, src: Source) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source: src,
            pid: None,
            seq: ts,
            payload: Payload::Mark {
                label: "m".into(),
                fields: Default::default(),
            },
        }
    }

    #[test]
    fn matches_inclusive_time_and_source() {
        let f = EventFilter {
            source: Some(Source::Mark),
            from_ts: Some(10),
            to_ts: Some(20),
            payload_kind: None,
        };
        assert!(f.matches(&ev(10, Source::Mark))); // == from inclusive
        assert!(f.matches(&ev(20, Source::Mark))); // == to inclusive
        assert!(!f.matches(&ev(9, Source::Mark)));
        assert!(!f.matches(&ev(21, Source::Mark)));
        assert!(!f.matches(&ev(15, Source::MetalHook))); // wrong source
    }

    #[test]
    fn empty_filter_matches_all() {
        let f = EventFilter {
            source: None,
            from_ts: None,
            to_ts: None,
            payload_kind: None,
        };
        assert!(f.matches(&ev(0, Source::Vm)));
    }

    #[test]
    fn chunk_overlaps_respects_time_and_bitmap() {
        let f = EventFilter {
            source: Some(Source::MetalHook),
            from_ts: Some(100),
            to_ts: Some(200),
            payload_kind: None,
        };
        let bm = 1u64 << Source::MetalHook.as_u8();
        assert!(f.chunk_overlaps(&ChunkIndexEntry {
            offset: 0,
            comp_len: 0,
            min_ts: 150,
            max_ts: 300,
            source_bitmap: bm,
            event_count: 1
        }));
        // time-disjoint:
        assert!(!f.chunk_overlaps(&ChunkIndexEntry {
            offset: 0,
            comp_len: 0,
            min_ts: 0,
            max_ts: 50,
            source_bitmap: bm,
            event_count: 1
        }));
        // source absent:
        assert!(!f.chunk_overlaps(&ChunkIndexEntry {
            offset: 0,
            comp_len: 0,
            min_ts: 150,
            max_ts: 160,
            source_bitmap: 1 << Source::Vm.as_u8(),
            event_count: 1
        }));
    }

    #[test]
    fn payload_kind_case_insensitive() {
        let f = EventFilter {
            source: None,
            from_ts: None,
            to_ts: None,
            payload_kind: Some("mark".into()),
        };
        assert!(f.matches(&ev(0, Source::Mark)));
    }

    #[test]
    fn payload_kind_mismatch() {
        let f = EventFilter {
            source: None,
            from_ts: None,
            to_ts: None,
            payload_kind: Some("VmSample".into()),
        };
        assert!(!f.matches(&ev(0, Source::Mark)));
    }
}

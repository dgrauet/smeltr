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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StackFrame {
    pub filename: String,
    pub lineno: u32,
    pub funcname: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum FieldValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpSample {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    pub gpu_ns: u64,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Payload {
    Mark {
        label: String,
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        fields: std::collections::BTreeMap<String, FieldValue>,
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
    MetalCbCommitted {
        cb_id: u64,
        queue_id: u64,
        queue_depth: u32,
        label: Option<String>,
    },
    MetalCbScheduled {
        cb_id: u64,
        queue_id: u64,
    },
    MetalCbCompleted {
        cb_id: u64,
        queue_id: u64,
        status: u32,
        error_code: Option<i64>,
        error_domain: Option<String>,
        in_flight_ns: u64,
    },
    MetalCbWarning {
        cb_id: u64,
        queue_id: u64,
        elapsed_ns: u64,
    },
    MetalCbOps {
        cb_id: u64,
        ops: Vec<OpSample>,
    },
    MetalHeapAlloc {
        heap_id: u64,
        size_bytes: u64,
        label: Option<String>,
    },
    MetalDeviceMemSample {
        allocated_bytes: u64,
        recommended_max_bytes: u64,
        at_event: String,
    },
    MetalHeapFree {
        heap_id: u64,
    },
    MetalBufferAlloc {
        buffer_id: u64,
        heap_id: Option<u64>,
        size_bytes: u64,
        label: Option<String>,
    },
    MetalBufferFree {
        buffer_id: u64,
    },
    MetalTextureAlloc {
        texture_id: u64,
        heap_id: Option<u64>,
        size_bytes: u64,
        label: Option<String>,
    },
    MetalTextureFree {
        texture_id: u64,
    },
    MetalHookDropped {
        count: u64,
    },
    MetalHookSkipped {
        reason: String,
    },
    MlxEvalEntered {
        call_id: u64,
        array_count: u32,
        stream: String,
        #[serde(default)]
        module_stack: Vec<u64>,
        #[serde(default)]
        stack_frames: Vec<StackFrame>,
    },
    MlxEvalReturned {
        call_id: u64,
        duration_ns: u64,
        was_async: bool,
    },
    MlxMemoryPoll {
        active_bytes: u64,
        peak_bytes: u64,
        cache_bytes: u64,
    },
    MlxArrayAlive {
        array_id: u64,
        size_bytes: u64,
        dtype: String,
        shape: Vec<u64>,
        stream: String,
    },
    MlxArrayFreed {
        array_id: u64,
    },
    MlxSnapshot {
        live_arrays: u32,
        total_array_bytes: u64,
        streams: Vec<String>,
        mlx_version: Option<String>,
    },
    MlxPanicTriggered {
        condition: String,
    },
    PythonSidecarHello {
        python_version: String,
        mlx_version: Option<String>,
        argv: Vec<String>,
    },
    PostMortemFlushed {
        reason: String,
        source_session: String,
        event_count: u32,
    },
    ProbeHealth {
        probe: String,
        state: ProbeHealthState,
        reason: Option<String>,
    },
    ModuleEntered {
        module_call_id: u64,
        module_def_id: u64,
        qualname: String,
        class_name: String,
        parent_call_id: Option<u64>,
        depth: u16,
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        fields: std::collections::BTreeMap<String, FieldValue>,
    },
    ModuleReturned {
        module_call_id: u64,
    },
    ModelLoad {
        /// Canonical (absolute, symlinks resolved) path to the loaded file.
        path: String,
        /// File size on disk in bytes.
        size_bytes: u64,
        /// Monotonic ns at the start of the load call.
        t_start_ns: u64,
        /// Monotonic ns at the end of the load call (close/return).
        t_end_ns: u64,
        /// First 8 hex chars of sha256(canonical_path) — stable color key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha8: Option<String>,
        /// "safetensors" | "mlx" | "torch" — for debugging.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        framework: Option<String>,
    },
}

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
        round_trip(
            Payload::Mark {
                label: "x".into(),
                fields: Default::default(),
            },
            Source::Mark,
        );
    }

    #[test]
    fn cbor_round_trip_mark_with_fields() {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("step".into(), FieldValue::Int(5));
        fields.insert("ok".into(), FieldValue::Bool(true));
        round_trip(
            Payload::Mark {
                label: "checkpoint".into(),
                fields,
            },
            Source::Mark,
        );
    }

    #[test]
    fn cbor_decodes_legacy_mark_without_fields() {
        round_trip(
            Payload::Mark {
                label: "plain".into(),
                fields: std::collections::BTreeMap::new(),
            },
            Source::Mark,
        );
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

    #[test]
    fn cbor_round_trip_metal_cb_committed() {
        round_trip(
            Payload::MetalCbCommitted {
                cb_id: 0xdead_beef,
                queue_id: 0x1438e0,
                queue_depth: 7,
                label: Some("eval".into()),
            },
            Source::MetalHook,
        );
    }

    #[test]
    fn cbor_round_trip_metal_cb_scheduled() {
        round_trip(
            Payload::MetalCbScheduled {
                cb_id: 1,
                queue_id: 2,
            },
            Source::MetalHook,
        );
    }

    #[test]
    fn cbor_round_trip_metal_cb_completed() {
        round_trip(
            Payload::MetalCbCompleted {
                cb_id: 1,
                queue_id: 2,
                status: 4,
                error_code: Some(0x0e),
                error_domain: Some("MTLCommandBufferErrorDomain".into()),
                in_flight_ns: 10_270_000_000,
            },
            Source::MetalHook,
        );
    }

    #[test]
    fn cbor_round_trip_metal_cb_warning() {
        round_trip(
            Payload::MetalCbWarning {
                cb_id: 1,
                queue_id: 2,
                elapsed_ns: 6_400_000_000,
            },
            Source::MetalHook,
        );
    }

    #[test]
    fn cbor_round_trip_metal_heap_alloc() {
        round_trip(
            Payload::MetalHeapAlloc {
                heap_id: 0x1a4,
                size_bytes: 7_500_000_000,
                label: None,
            },
            Source::MetalHook,
        );
    }

    #[test]
    fn cbor_round_trip_metal_heap_free() {
        round_trip(Payload::MetalHeapFree { heap_id: 0x1a4 }, Source::MetalHook);
    }

    #[test]
    fn cbor_round_trip_metal_buffer_alloc() {
        round_trip(
            Payload::MetalBufferAlloc {
                buffer_id: 0xb1,
                heap_id: Some(0x1a4),
                size_bytes: 4096,
                label: Some("video_embeds".into()),
            },
            Source::MetalHook,
        );
    }

    #[test]
    fn cbor_round_trip_metal_buffer_free() {
        round_trip(
            Payload::MetalBufferFree { buffer_id: 0xb1 },
            Source::MetalHook,
        );
    }

    #[test]
    fn cbor_round_trip_metal_texture_alloc() {
        round_trip(
            Payload::MetalTextureAlloc {
                texture_id: 0xa1b2,
                heap_id: None,
                size_bytes: 16384,
                label: None,
            },
            Source::MetalHook,
        );
    }

    #[test]
    fn cbor_round_trip_metal_texture_free() {
        round_trip(
            Payload::MetalTextureFree { texture_id: 0xa1b2 },
            Source::MetalHook,
        );
    }

    #[test]
    fn cbor_round_trip_metal_hook_dropped() {
        round_trip(Payload::MetalHookDropped { count: 42 }, Source::MetalHook);
    }

    #[test]
    fn cbor_round_trip_metal_hook_skipped() {
        round_trip(
            Payload::MetalHookSkipped {
                reason: "macOS 27 untested".into(),
            },
            Source::MetalHook,
        );
    }

    #[test]
    fn cbor_round_trip_mlx_eval_entered() {
        round_trip(
            Payload::MlxEvalEntered {
                call_id: 17,
                array_count: 3,
                stream: "gpu".into(),
                module_stack: vec![1, 2, 3],
                stack_frames: vec![],
            },
            Source::PythonSidecar,
        );
    }

    #[test]
    fn cbor_decodes_legacy_mlx_eval_entered_without_module_stack() {
        let legacy = Payload::MlxEvalEntered {
            call_id: 7,
            array_count: 1,
            stream: "gpu".into(),
            module_stack: Vec::new(),
            stack_frames: Vec::new(),
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&legacy, &mut buf).unwrap();
        let value: ciborium::value::Value = ciborium::de::from_reader(&buf[..]).unwrap();
        let stripped = match value {
            ciborium::value::Value::Map(pairs) => ciborium::value::Value::Map(
                pairs
                    .into_iter()
                    .filter(|(k, _)| {
                        !matches!(k, ciborium::value::Value::Text(s) if s == "module_stack")
                    })
                    .collect(),
            ),
            v => v,
        };
        let mut stripped_buf = Vec::new();
        ciborium::ser::into_writer(&stripped, &mut stripped_buf).unwrap();

        let decoded: Payload = ciborium::de::from_reader(&stripped_buf[..]).unwrap();
        match decoded {
            Payload::MlxEvalEntered { module_stack, .. } => {
                assert!(module_stack.is_empty());
            }
            other => panic!("expected MlxEvalEntered, got {other:?}"),
        }
    }

    #[test]
    fn cbor_round_trip_mlx_eval_returned() {
        round_trip(
            Payload::MlxEvalReturned {
                call_id: 17,
                duration_ns: 4_200_000,
                was_async: true,
            },
            Source::PythonSidecar,
        );
    }

    #[test]
    fn cbor_round_trip_mlx_memory_poll() {
        round_trip(
            Payload::MlxMemoryPoll {
                active_bytes: 12_345,
                peak_bytes: 99_999,
                cache_bytes: 4_096,
            },
            Source::PythonSidecar,
        );
    }

    #[test]
    fn cbor_round_trip_mlx_array_alive() {
        round_trip(
            Payload::MlxArrayAlive {
                array_id: 0xdead_beef,
                size_bytes: 1024,
                dtype: "float16".into(),
                shape: vec![1, 64, 64, 3],
                stream: "gpu".into(),
            },
            Source::PythonSidecar,
        );
    }

    #[test]
    fn cbor_round_trip_mlx_array_freed() {
        round_trip(
            Payload::MlxArrayFreed {
                array_id: 0xdead_beef,
            },
            Source::PythonSidecar,
        );
    }

    #[test]
    fn cbor_round_trip_mlx_snapshot() {
        round_trip(
            Payload::MlxSnapshot {
                live_arrays: 42,
                total_array_bytes: 16_777_216,
                streams: vec!["gpu".into(), "cpu".into()],
                mlx_version: Some("0.18.1".into()),
            },
            Source::PythonSidecar,
        );
    }

    #[test]
    fn cbor_round_trip_mlx_panic_triggered() {
        round_trip(
            Payload::MlxPanicTriggered {
                condition: "active_bytes > 20GiB".into(),
            },
            Source::PythonSidecar,
        );
    }

    #[test]
    fn cbor_round_trip_post_mortem_flushed() {
        round_trip(
            Payload::PostMortemFlushed {
                reason: "MetalCbCompleted error=14".into(),
                source_session: "abcdef12".into(),
                event_count: 1234,
            },
            Source::System,
        );
    }

    #[test]
    fn cbor_round_trip_python_sidecar_hello() {
        round_trip(
            Payload::PythonSidecarHello {
                python_version: "3.12.1".into(),
                mlx_version: Some("0.18.1".into()),
                argv: vec!["python".into(), "my_script.py".into()],
            },
            Source::PythonSidecar,
        );
    }

    #[test]
    fn cbor_round_trip_module_entered() {
        round_trip(
            Payload::ModuleEntered {
                module_call_id: 17,
                module_def_id: 0xdead_beef,
                qualname: "TransformerBlock.attention.qkv_proj".into(),
                class_name: "Linear".into(),
                parent_call_id: Some(3),
                depth: 4,
                fields: Default::default(),
            },
            Source::PythonSidecar,
        );
    }

    #[test]
    fn cbor_round_trip_module_returned() {
        round_trip(
            Payload::ModuleReturned { module_call_id: 17 },
            Source::PythonSidecar,
        );
    }

    #[test]
    fn cbor_round_trip_op_sample() {
        let s = OpSample {
            name: "Matmul".into(),
            symbol: None,
            gpu_ns: 1_234_567,
            count: 3,
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&s, &mut buf).unwrap();
        let decoded: OpSample = ciborium::from_reader(&buf[..]).unwrap();
        assert_eq!(s, decoded);
    }

    #[test]
    fn cbor_round_trip_metal_cb_ops() {
        round_trip(
            Payload::MetalCbOps {
                cb_id: 0xdead_beef,
                ops: vec![
                    OpSample {
                        name: "Matmul".into(),
                        symbol: None,
                        gpu_ns: 6_200_000,
                        count: 3,
                    },
                    OpSample {
                        name: "Softmax".into(),
                        symbol: None,
                        gpu_ns: 1_500_000,
                        count: 1,
                    },
                    OpSample {
                        name: "RMSNorm".into(),
                        symbol: None,
                        gpu_ns: 400_000,
                        count: 2,
                    },
                ],
            },
            Source::MetalHook,
        );
    }

    #[test]
    fn cbor_round_trip_metal_cb_ops_empty() {
        round_trip(
            Payload::MetalCbOps {
                cb_id: 1,
                ops: vec![],
            },
            Source::MetalHook,
        );
    }

    #[test]
    fn opsample_round_trip_with_symbol() {
        let s = OpSample {
            name: "K_f900_64x64x1".into(),
            symbol: Some("gemm_t_n_bf16_64_64_32_2_2_8".into()),
            gpu_ns: 12345,
            count: 7,
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&s, &mut buf).unwrap();
        let decoded: OpSample = ciborium::from_reader(&buf[..]).unwrap();
        assert_eq!(s, decoded);
    }

    #[test]
    fn opsample_round_trip_without_symbol_is_compact() {
        let s = OpSample {
            name: "K_f900_64x64x1".into(),
            symbol: None,
            gpu_ns: 12345,
            count: 7,
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&s, &mut buf).unwrap();
        let v: ciborium::Value = ciborium::from_reader(&buf[..]).unwrap();
        let map = v.as_map().expect("OpSample must serialize as a CBOR map");
        let has_symbol_key = map
            .iter()
            .any(|(k, _)| k.as_text().map(|t| t == "symbol").unwrap_or(false));
        assert!(!has_symbol_key, "symbol=None must not emit a key");
        let decoded: OpSample = ciborium::from_reader(&buf[..]).unwrap();
        assert_eq!(s, decoded);
    }

    #[test]
    fn opsample_legacy_cbor_decodes_with_none_symbol() {
        let legacy = ciborium::Value::Map(vec![
            (
                ciborium::Value::Text("name".into()),
                ciborium::Value::Text("K_old".into()),
            ),
            (
                ciborium::Value::Text("gpu_ns".into()),
                ciborium::Value::Integer(99u64.into()),
            ),
            (
                ciborium::Value::Text("count".into()),
                ciborium::Value::Integer(3u32.into()),
            ),
        ]);
        let mut buf = Vec::new();
        ciborium::into_writer(&legacy, &mut buf).unwrap();
        let decoded: OpSample = ciborium::from_reader(&buf[..]).unwrap();
        assert_eq!(decoded.name, "K_old");
        assert_eq!(decoded.symbol, None);
        assert_eq!(decoded.gpu_ns, 99);
        assert_eq!(decoded.count, 3);
    }

    #[test]
    fn cbor_round_trip_metal_device_mem_sample() {
        round_trip(
            Payload::MetalDeviceMemSample {
                allocated_bytes: 8_589_934_592,
                recommended_max_bytes: 17_179_869_184,
                at_event: "cb_committed".into(),
            },
            Source::MetalHook,
        );
    }

    #[test]
    fn cbor_round_trip_mlx_eval_entered_with_stack_frames() {
        round_trip(
            Payload::MlxEvalEntered {
                call_id: 1,
                array_count: 3,
                stream: "gpu".into(),
                module_stack: vec![1, 2],
                stack_frames: vec![
                    StackFrame {
                        filename: "attention.py".into(),
                        lineno: 127,
                        funcname: "forward".into(),
                    },
                    StackFrame {
                        filename: "model.py".into(),
                        lineno: 42,
                        funcname: "__call__".into(),
                    },
                ],
            },
            Source::PythonSidecar,
        );
    }

    #[test]
    fn cbor_round_trip_module_entered_with_fields() {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("step".to_string(), FieldValue::Int(5));
        fields.insert("sigma".to_string(), FieldValue::Float(0.5));
        fields.insert("tag".to_string(), FieldValue::String("a".into()));
        fields.insert("ok".to_string(), FieldValue::Bool(true));
        round_trip(
            Payload::ModuleEntered {
                module_call_id: 1,
                module_def_id: 100,
                qualname: "denoise.step".into(),
                class_name: "Scope".into(),
                parent_call_id: None,
                depth: 0,
                fields,
            },
            Source::PythonSidecar,
        );
    }

    #[test]
    fn cbor_decodes_legacy_module_entered_without_fields() {
        let legacy = Payload::ModuleEntered {
            module_call_id: 1,
            module_def_id: 100,
            qualname: "old".into(),
            class_name: "Module".into(),
            parent_call_id: None,
            depth: 0,
            fields: std::collections::BTreeMap::new(),
        };
        round_trip(legacy, Source::PythonSidecar);
    }

    #[test]
    fn cbor_round_trip_field_value_variants() {
        use FieldValue::*;
        for v in [Bool(false), Int(-42), Float(1.5), String("x".into())] {
            let mut fields = std::collections::BTreeMap::new();
            fields.insert("v".to_string(), v);
            round_trip(
                Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 0,
                    qualname: "x".into(),
                    class_name: "x".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields,
                },
                Source::PythonSidecar,
            );
        }
    }

    #[test]
    fn cbor_round_trip_model_load_full() {
        round_trip(
            Payload::ModelLoad {
                path: "/models/gemma-2b/model.safetensors".into(),
                size_bytes: 2_147_483_648,
                t_start_ns: 1_000_000_000,
                t_end_ns: 1_500_000_000,
                sha8: Some("deadbeef".into()),
                framework: Some("safetensors".into()),
            },
            Source::PythonSidecar,
        );
    }

    #[test]
    fn cbor_round_trip_model_load_minimal() {
        round_trip(
            Payload::ModelLoad {
                path: "/models/model.safetensors".into(),
                size_bytes: 1_073_741_824,
                t_start_ns: 2_000_000_000,
                t_end_ns: 2_800_000_000,
                sha8: None,
                framework: None,
            },
            Source::PythonSidecar,
        );
    }

    #[test]
    fn cbor_model_load_optional_fields_not_serialized_when_none() {
        let p = Payload::ModelLoad {
            path: "/tmp/x.safetensors".into(),
            size_bytes: 100,
            t_start_ns: 1,
            t_end_ns: 2,
            sha8: None,
            framework: None,
        };
        let mut buf = Vec::new();
        ciborium::into_writer(&p, &mut buf).unwrap();
        let v: ciborium::Value = ciborium::from_reader(&buf[..]).unwrap();
        let map = v.as_map().expect("must serialize as CBOR map");
        for key in ["sha8", "framework"] {
            assert!(
                !map.iter()
                    .any(|(k, _)| k.as_text().map(|t| t == key).unwrap_or(false)),
                "{key}=None must not emit a key"
            );
        }
    }

    #[test]
    fn cbor_decodes_legacy_model_load_without_sha8_and_framework() {
        // Hand-craft a CBOR map without the optional sha8 / framework fields.
        let legacy = ciborium::Value::Map(vec![
            (
                ciborium::Value::Text("kind".into()),
                ciborium::Value::Text("ModelLoad".into()),
            ),
            (
                ciborium::Value::Text("path".into()),
                ciborium::Value::Text("/models/old.safetensors".into()),
            ),
            (
                ciborium::Value::Text("size_bytes".into()),
                ciborium::Value::Integer(500u64.into()),
            ),
            (
                ciborium::Value::Text("t_start_ns".into()),
                ciborium::Value::Integer(10u64.into()),
            ),
            (
                ciborium::Value::Text("t_end_ns".into()),
                ciborium::Value::Integer(20u64.into()),
            ),
        ]);
        let mut buf = Vec::new();
        ciborium::into_writer(&legacy, &mut buf).unwrap();
        let decoded: Payload = ciborium::from_reader(&buf[..]).unwrap();
        match decoded {
            Payload::ModelLoad {
                sha8, framework, ..
            } => {
                assert_eq!(sha8, None);
                assert_eq!(framework, None);
            }
            other => panic!("expected ModelLoad, got {other:?}"),
        }
    }

    #[test]
    fn cbor_decodes_legacy_mlx_eval_entered_without_stack_frames() {
        // Hand-craft a legacy CBOR map missing stack_frames.
        let legacy = ciborium::Value::Map(vec![
            (
                ciborium::Value::Text("kind".into()),
                ciborium::Value::Text("MlxEvalEntered".into()),
            ),
            (
                ciborium::Value::Text("call_id".into()),
                ciborium::Value::Integer(1u64.into()),
            ),
            (
                ciborium::Value::Text("array_count".into()),
                ciborium::Value::Integer(3u64.into()),
            ),
            (
                ciborium::Value::Text("stream".into()),
                ciborium::Value::Text("gpu".into()),
            ),
            (
                ciborium::Value::Text("module_stack".into()),
                ciborium::Value::Array(vec![]),
            ),
        ]);
        let mut buf = Vec::new();
        ciborium::into_writer(&legacy, &mut buf).unwrap();
        let decoded: Payload = ciborium::from_reader(&buf[..]).unwrap();
        match decoded {
            Payload::MlxEvalEntered { stack_frames, .. } => {
                assert!(stack_frames.is_empty());
            }
            other => panic!("expected MlxEvalEntered, got {other:?}"),
        }
    }
}

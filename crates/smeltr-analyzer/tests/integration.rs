//! Golden fixture: ensure analyze() on a synthetic watchdog trace produces
//! the four items required by spec section 2.2 / 7.4.

use smeltr_analyzer::{analyze, Category, Severity};
use smeltr_core::event::Event;

#[test]
fn synthetic_watchdog_yields_all_four_done_items() {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/synthetic-watchdog.json");
    let text = std::fs::read_to_string(&path).expect("read fixture");
    let events: Vec<Event> = serde_json::from_str(&text).expect("parse fixture");
    let report = analyze(&events);

    // 1. kIOGPUCommandBufferCallback* code is named.
    let rc = report.root_cause().expect("missing root cause");
    assert_eq!(rc.severity, Severity::Critical);
    assert_eq!(rc.category, Category::RootCause);
    assert!(
        rc.title.contains("ImpactingInteractivity"),
        "root cause title was {:?}",
        rc.title
    );

    // 2. Queue depth at crash is reported.
    let queue = report
        .contributing_factors()
        .find(|f| f.title.contains("Queue depth peaked"))
        .expect("missing queue depth finding");
    assert!(
        queue.title.contains("23"),
        "queue title was {:?}",
        queue.title
    );

    // 3. mx.eval timing relative to crash.
    let timing = report
        .timing()
        .find(|f| f.title.contains("call_id=1"))
        .expect("missing mx.eval timing finding");
    assert!(
        timing.detail.contains("before crash"),
        "timing detail was {:?}",
        timing.detail
    );

    // 4. System pressure: ReportCrash + diagnosticservicesd above threshold.
    let pressures: Vec<_> = report.system_pressure().collect();
    let names: Vec<&str> = pressures.iter().map(|f| f.title.as_str()).collect();
    assert!(
        names.iter().any(|t| t.contains("ReportCrash")),
        "expected ReportCrash flag, got {:?}",
        names
    );
    assert!(
        names.iter().any(|t| t.contains("diagnosticservicesd")),
        "expected diagnosticservicesd flag, got {:?}",
        names
    );

    // Render the full report — must include all sections.
    let text = report.render();
    assert!(text.contains("ROOT CAUSE"));
    assert!(text.contains("CONTRIBUTING FACTORS"));
    assert!(text.contains("TIMING"));
    assert!(text.contains("SYSTEM PRESSURE"));
}

#[test]
fn queue_pressure_fires_on_no_crash_high_depth() {
    use smeltr_analyzer::{analyze, Category, Severity};
    use smeltr_core::event::{Event, Payload, Source};
    use uuid::Uuid;

    let mut events: Vec<Event> = (1u32..=40)
        .map(|d| Event {
            ts_mono_ns: (d as u64) * 100_000_000,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq: d as u64,
            payload: Payload::MetalCbCommitted {
                cb_id: d as u64,
                queue_id: 1,
                queue_depth: d,
                label: None,
            },
        })
        .collect();
    events.push(Event {
        ts_mono_ns: 50 * 100_000_000,
        ts_wall_ns: 0,
        session_id: Uuid::nil(),
        source: Source::MetalHook,
        pid: None,
        seq: 999,
        payload: Payload::MetalCbCompleted {
            cb_id: 40,
            queue_id: 1,
            status: 4,
            error_code: None,
            error_domain: None,
            in_flight_ns: 1_500_000_000,
        },
    });

    let report = analyze(&events);
    let pressure = report
        .contributing_factors()
        .find(|f| f.title.contains("Queue pressure"));
    assert!(pressure.is_some(), "expected Queue pressure finding");
    let p = pressure.unwrap();
    assert_eq!(p.category, Category::ContributingFactor);
    assert_eq!(p.severity, Severity::Warning);
}

#[test]
fn queue_pressure_silent_on_modest_workload() {
    use smeltr_analyzer::analyze;
    use smeltr_core::event::{Event, Payload, Source};
    use uuid::Uuid;

    let events: Vec<Event> = (1u32..=5)
        .flat_map(|d| {
            vec![
                Event {
                    ts_mono_ns: (d as u64) * 1_000_000,
                    ts_wall_ns: 0,
                    session_id: Uuid::nil(),
                    source: Source::MetalHook,
                    pid: None,
                    seq: d as u64,
                    payload: Payload::MetalCbCommitted {
                        cb_id: d as u64,
                        queue_id: 1,
                        queue_depth: d,
                        label: None,
                    },
                },
                Event {
                    ts_mono_ns: (d as u64) * 1_000_000 + 500_000,
                    ts_wall_ns: 0,
                    session_id: Uuid::nil(),
                    source: Source::MetalHook,
                    pid: None,
                    seq: 100 + d as u64,
                    payload: Payload::MetalCbCompleted {
                        cb_id: d as u64,
                        queue_id: 1,
                        status: 4,
                        error_code: None,
                        error_domain: None,
                        in_flight_ns: 100_000_000,
                    },
                },
            ]
        })
        .collect();

    let report = analyze(&events);
    let pressure = report
        .contributing_factors()
        .find(|f| f.title.contains("Queue pressure"));
    assert!(
        pressure.is_none(),
        "unexpected Queue pressure finding on modest workload"
    );
}

#[test]
fn queue_pressure_defers_to_queue_depth_on_crash() {
    use smeltr_analyzer::analyze;
    use smeltr_core::event::{Event, Payload, Source};
    use uuid::Uuid;

    let mut events: Vec<Event> = (1u32..=40)
        .map(|d| Event {
            ts_mono_ns: (d as u64) * 100_000_000,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::MetalHook,
            pid: None,
            seq: d as u64,
            payload: Payload::MetalCbCommitted {
                cb_id: d as u64,
                queue_id: 1,
                queue_depth: d,
                label: None,
            },
        })
        .collect();
    events.push(Event {
        ts_mono_ns: 50 * 100_000_000,
        ts_wall_ns: 0,
        session_id: Uuid::nil(),
        source: Source::MetalHook,
        pid: None,
        seq: 999,
        payload: Payload::MetalCbCompleted {
            cb_id: 40,
            queue_id: 1,
            status: 4,
            error_code: Some(14),
            error_domain: Some("IOGPU".into()),
            in_flight_ns: 1_500_000_000,
        },
    });

    let report = analyze(&events);
    let queue_depth = report
        .contributing_factors()
        .find(|f| f.title.contains("Queue depth peaked"));
    assert!(
        queue_depth.is_some(),
        "QueueDepthRule should still fire on crash"
    );
    let pressure = report
        .contributing_factors()
        .find(|f| f.title.contains("Queue pressure"));
    assert!(
        pressure.is_none(),
        "QueuePressureRule should defer when crash present, got: {:?}",
        pressure
    );
}

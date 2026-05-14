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

use ratatui::backend::TestBackend;
use ratatui::Terminal;
use smeltr_core::event::{Event, OpSample, Payload, Source};
use smeltr_tui::render::{render, Panel, RenderCtx};
use smeltr_tui::state::UiState;
use uuid::Uuid;

fn ev(ts: u64, payload: Payload) -> Event {
    Event {
        ts_mono_ns: ts,
        ts_wall_ns: ts,
        session_id: Uuid::nil(),
        source: Source::Mark,
        pid: None,
        seq: ts,
        payload,
    }
}

fn dump(buffer: &ratatui::buffer::Buffer) -> String {
    let mut out = String::new();
    let area = buffer.area;
    for y in 0..area.height {
        for x in 0..area.width {
            let cell = &buffer[(area.x + x, area.y + y)];
            out.push_str(cell.symbol());
        }
        out.push('\n');
    }
    out
}

#[test]
fn render_after_synthetic_events_shows_all_panels() {
    let mut state = UiState::default();
    state.ingest(&ev(
        1_000_000_000,
        Payload::Mark {
            label: "phase: encode".into(),
            fields: Default::default(),
        },
    ));
    state.ingest(&ev(
        1_500_000_000,
        Payload::MetalCbCommitted {
            cb_id: 100,
            queue_id: 1,
            queue_depth: 5,
            label: None,
        },
    ));
    state.ingest(&ev(
        2_000_000_000,
        Payload::MlxMemoryPoll {
            active_bytes: 14 * 1024 * 1024 * 1024,
            peak_bytes: 18 * 1024 * 1024 * 1024,
            cache_bytes: 2 * 1024 * 1024 * 1024,
        },
    ));

    let backend = TestBackend::new(120, 40);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| {
        render(
            f,
            &state,
            RenderCtx {
                focus: Panel::Timeline,
                paused: false,
                mode_label: "live",
                show_hot_kernels: false,
                show_models: false,
            },
        )
    })
    .unwrap();
    let buf = term.backend().buffer();
    let d = dump(buf);
    for marker in [
        "smeltr",
        "session:",
        "events:",
        "Metal CBs",
        "Memory",
        "MLX (sidecar)",
        "System pressure",
        "Notices",
        "phase: encode",
        "depth=5",
    ] {
        assert!(d.contains(marker), "missing {:?} in dump:\n{d}", marker);
    }
}

#[test]
fn hot_kernels_panel_renders_when_toggled_on() {
    let mut state = UiState::default();
    state.ingest(&ev(
        1_000_000_000,
        Payload::MetalCbOps {
            cb_id: 1,
            ops: vec![OpSample {
                name: "K_demo_8x8x1".into(),
                symbol: None,
                gpu_ns: 250_000,
                count: 3,
            }],
        },
    ));

    let backend = TestBackend::new(120, 50);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| {
        render(
            f,
            &state,
            RenderCtx {
                focus: Panel::Timeline,
                paused: false,
                mode_label: "live",
                show_hot_kernels: true,
                show_models: false,
            },
        )
    })
    .unwrap();
    let buf = term.backend().buffer();
    let d = dump(buf);
    for marker in ["Hot kernels", "K_demo_8x8x1"] {
        assert!(d.contains(marker), "missing {:?} in dump:\n{d}", marker);
    }
}

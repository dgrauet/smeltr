use ratatui::backend::TestBackend;
use ratatui::Terminal;
use smeltr_analyzer::{Diagnostics, ModuleBreakdown};
use smeltr_tui::breakdown::{render, BreakdownState};

#[test]
fn breakdown_renders_qualnames() {
    let mut state = BreakdownState {
        root: Some(ModuleBreakdown {
            qualname: "<root>".into(),
            class_name: "".into(),
            calls: 0,
            gpu_ns_self: 0,
            gpu_ns_subtree: 1500,
            eval_count: 0,
            cb_count: 0,
            children: vec![ModuleBreakdown {
                qualname: "Linear".into(),
                class_name: "Linear".into(),
                calls: 1,
                gpu_ns_self: 1500,
                gpu_ns_subtree: 1500,
                eval_count: 1,
                cb_count: 1,
                children: vec![],
                diagnostics: None,
            }],
            diagnostics: Some(Diagnostics::default()),
        }),
        ..Default::default()
    };
    let backend = TestBackend::new(120, 12);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| {
        let area = f.area();
        render(f, area, &mut state);
    })
    .unwrap();
    let buffer = term.backend().buffer();
    let dump = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(dump.contains("Linear"), "dump:\n{dump}");
}

#[test]
fn breakdown_with_none_root_shows_placeholder() {
    let mut state = BreakdownState::default();
    let backend = TestBackend::new(120, 6);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| {
        let area = f.area();
        render(f, area, &mut state);
    })
    .unwrap();
    let buffer = term.backend().buffer();
    let dump = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(dump.contains("not loaded"), "dump:\n{dump}");
}

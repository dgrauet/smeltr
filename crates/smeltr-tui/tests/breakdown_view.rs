use ratatui::backend::TestBackend;
use ratatui::Terminal;
use smeltr_analyzer::{Diagnostics, ModuleBreakdown, OpAttribution};
use smeltr_tui::breakdown::{handle_key, render, BreakdownState};

fn dump(buf: &ratatui::buffer::Buffer) -> String {
    (0..buf.area.height)
        .map(|y| {
            (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn fixture_with_op() -> ModuleBreakdown {
    ModuleBreakdown {
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
            ops: vec![OpAttribution {
                name: "Matmul".into(),
                gpu_ns: 1200,
                count: 1,
                symbol: None,
                kind: None,
            }],
            diagnostics: None,
        }],
        ops: vec![],
        diagnostics: None,
    }
}

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
                ops: vec![],
                diagnostics: None,
            }],
            ops: vec![],
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
    let d = dump(term.backend().buffer());
    assert!(d.contains("Linear"), "dump:\n{d}");
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
    let d = dump(term.backend().buffer());
    assert!(d.contains("not loaded"), "dump:\n{d}");
}

#[test]
fn breakdown_view_renders_ops_when_selected() {
    let mut state = BreakdownState {
        root: Some(fixture_with_op()),
        ..Default::default()
    };
    state.list_state.select(Some(0)); // Linear
    let backend = TestBackend::new(140, 12);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| {
        let area = f.area();
        render(f, area, &mut state);
    })
    .unwrap();
    let d = dump(term.backend().buffer());
    assert!(d.contains("Matmul"), "expected Matmul in op panel:\n{d}");
}

#[test]
fn breakdown_view_o_toggle_hides_ops() {
    let mut state = BreakdownState {
        root: Some(fixture_with_op()),
        ..Default::default()
    };
    state.list_state.select(Some(0));
    assert!(state.show_ops, "default should be true");
    assert!(handle_key(&mut state, 'O'));
    assert!(!state.show_ops);

    let backend = TestBackend::new(140, 12);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| {
        let area = f.area();
        render(f, area, &mut state);
    })
    .unwrap();
    let d = dump(term.backend().buffer());
    assert!(
        !d.contains("Matmul"),
        "Matmul should be hidden when show_ops=false:\n{d}"
    );
}

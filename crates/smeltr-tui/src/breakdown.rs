//! Breakdown view: tree of ModuleBreakdown.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use ratatui::Frame;
use smeltr_analyzer::ModuleBreakdown;

#[derive(Default)]
pub struct BreakdownState {
    pub root: Option<ModuleBreakdown>,
    pub list_state: ListState,
}

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &mut BreakdownState) {
    let items: Vec<ListItem> = match &state.root {
        None => vec![ListItem::new(
            "breakdown not loaded - compute via smeltr_analyzer::compute_breakdown",
        )],
        Some(root) => {
            let mut rows: Vec<(u16, &ModuleBreakdown)> = Vec::new();
            fn walk<'a>(
                n: &'a ModuleBreakdown,
                depth: u16,
                out: &mut Vec<(u16, &'a ModuleBreakdown)>,
            ) {
                out.push((depth, n));
                for c in &n.children {
                    walk(c, depth + 1, out);
                }
            }
            for c in &root.children {
                walk(c, 0, &mut rows);
            }
            rows.into_iter()
                .map(|(d, n)| {
                    let indent = "  ".repeat(d as usize);
                    let line = format!(
                        "{indent}{:<40} {:>10.3}us self  {:>10.3}us subtree  calls={}",
                        n.qualname,
                        n.gpu_ns_self as f64 / 1000.0,
                        n.gpu_ns_subtree as f64 / 1000.0,
                        n.calls,
                    );
                    ListItem::new(Line::from(Span::styled(line, Style::default())))
                })
                .collect()
        }
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Inference breakdown"),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut state.list_state);
}

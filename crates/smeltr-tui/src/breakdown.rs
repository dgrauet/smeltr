//! Breakdown view: tree of ModuleBreakdown.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use ratatui::Frame;
use smeltr_analyzer::ModuleBreakdown;

pub struct BreakdownState {
    pub root: Option<ModuleBreakdown>,
    pub list_state: ListState,
    pub show_ops: bool,
}

impl Default for BreakdownState {
    fn default() -> Self {
        Self {
            root: None,
            list_state: ListState::default(),
            show_ops: true,
        }
    }
}

/// Returns true if the key was consumed by the breakdown view.
pub fn handle_key(state: &mut BreakdownState, key: char) -> bool {
    match key {
        'O' | 'o' => {
            state.show_ops = !state.show_ops;
            true
        }
        _ => false,
    }
}

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &mut BreakdownState) {
    let split = if state.show_ops {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100)])
            .split(area)
    };
    let modules_area = split[0];

    let rows: Vec<(u16, &ModuleBreakdown)> = match &state.root {
        None => Vec::new(),
        Some(root) => {
            let mut out = Vec::new();
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
                walk(c, 0, &mut out);
            }
            out
        }
    };

    let items: Vec<ListItem> = if rows.is_empty() {
        vec![ListItem::new(
            "breakdown not loaded - compute via smeltr_analyzer::compute_breakdown",
        )]
    } else {
        rows.iter()
            .map(|(d, n)| {
                let indent = "  ".repeat(*d as usize);
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
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Inference breakdown"),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, modules_area, &mut state.list_state);

    if state.show_ops {
        let ops_area = split[1];
        let selected_node = state
            .list_state
            .selected()
            .and_then(|i| rows.get(i))
            .map(|(_, n)| *n);
        let op_items: Vec<ListItem> = match selected_node {
            Some(n) if !n.ops.is_empty() => n
                .ops
                .iter()
                .take(5)
                .map(|op| {
                    ListItem::new(format!(
                        "{:<16} {:>10}us  cnt={}",
                        op.name,
                        op.gpu_ns / 1000,
                        op.count,
                    ))
                })
                .collect(),
            Some(_) => vec![ListItem::new("(no ops)")],
            None => vec![ListItem::new("(select a module)")],
        };
        let ops_list =
            List::new(op_items).block(Block::default().borders(Borders::ALL).title("Top ops"));
        frame.render_widget(ops_list, ops_area);
    }
}

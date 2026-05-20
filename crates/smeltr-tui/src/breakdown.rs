//! Breakdown view: tree of ModuleBreakdown.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use ratatui::Frame;
use smeltr_analyzer::ModuleBreakdown;
use smeltr_core::event::FieldValue;
use std::collections::BTreeMap;

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

/// Format a fields map for inline display: `[key1=val1, key2=val2]`.
/// Returns empty string when the map is empty.
fn format_fields(fields: &BTreeMap<String, FieldValue>) -> String {
    if fields.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = fields
        .iter()
        .map(|(k, v)| format!("{k}={}", format_field_value(v)))
        .collect();
    format!(" [{}]", parts.join(", "))
}

fn format_field_value(v: &FieldValue) -> String {
    match v {
        FieldValue::Bool(b) => b.to_string(),
        FieldValue::Int(i) => i.to_string(),
        FieldValue::Float(f) => format!("{f:.4}"),
        FieldValue::String(s) => s.clone(),
    }
}

/// Format an op row, preferring `symbol` over `name` and appending the
/// resolved `kind` in brackets when present.
fn format_op_label(op: &smeltr_analyzer::OpAttribution) -> String {
    let primary = op.symbol.as_deref().unwrap_or(&op.name);
    match op.kind.as_deref() {
        Some(k) => format!("{primary} [{k}]"),
        None => primary.to_string(),
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
                let label = format!("{}{}", n.qualname, format_fields(&n.fields));
                let line = format!(
                    "{indent}{:<48} {:>10.3}us self  {:>10.3}us subtree  calls={}",
                    label,
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
                        "{:<32} {:>10}us  cnt={}",
                        format_op_label(op),
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

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::FieldValue;

    #[test]
    fn format_fields_renders_bracketed_list() {
        let mut m = BTreeMap::new();
        m.insert("step".into(), FieldValue::Int(5));
        m.insert("kind".into(), FieldValue::String("matmul".into()));
        // BTreeMap iterates sorted — `kind` comes before `step`.
        assert_eq!(format_fields(&m), " [kind=matmul, step=5]");
    }

    #[test]
    fn format_fields_empty_returns_empty_string() {
        let m: BTreeMap<String, FieldValue> = BTreeMap::new();
        assert_eq!(format_fields(&m), "");
    }

    #[test]
    fn format_field_value_bool_int_float_string() {
        assert_eq!(format_field_value(&FieldValue::Bool(true)), "true");
        assert_eq!(format_field_value(&FieldValue::Int(-42)), "-42");
        assert_eq!(format_field_value(&FieldValue::Float(0.5)), "0.5000");
        assert_eq!(format_field_value(&FieldValue::String("x".into())), "x");
    }

    #[test]
    fn format_op_label_prefers_symbol_over_name() {
        let op = smeltr_analyzer::OpAttribution {
            name: "K_xxx".into(),
            symbol: Some("gemm_t_n_bf16".into()),
            kind: Some("Matmul".into()),
            gpu_ns: 1000,
            count: 1,
        };
        assert_eq!(format_op_label(&op), "gemm_t_n_bf16 [Matmul]");
    }

    #[test]
    fn format_op_label_falls_back_to_name_when_no_symbol() {
        let op = smeltr_analyzer::OpAttribution {
            name: "K_xxx".into(),
            symbol: None,
            kind: None,
            gpu_ns: 1000,
            count: 1,
        };
        assert_eq!(format_op_label(&op), "K_xxx");
    }

    #[test]
    fn format_op_label_symbol_no_kind() {
        let op = smeltr_analyzer::OpAttribution {
            name: "K_xxx".into(),
            symbol: Some("unknown_kernel".into()),
            kind: None,
            gpu_ns: 1000,
            count: 1,
        };
        assert_eq!(format_op_label(&op), "unknown_kernel");
    }
}

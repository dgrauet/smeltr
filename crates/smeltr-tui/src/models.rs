//! TUI "Models" view — swim lanes per loaded safetensors/MLX file plus a
//! cumulative GPU-memory line chart.

use crate::state::UiState;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols::Marker;
use ratatui::text::Line;
use ratatui::widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph};
use ratatui::Frame;
use std::path::Path;

const MAX_MODEL_ROWS: usize = 12;

/// Stable colour for a string key (sha8 or path fallback).
fn color_for(key: &str) -> Color {
    let palette = [
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::LightRed,
        Color::LightGreen,
    ];
    let h = key
        .bytes()
        .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    palette[(h as usize) % palette.len()]
}

fn human_bytes(b: u64) -> String {
    const KB: u64 = 1_024;
    const MB: u64 = KB * 1_024;
    const GB: u64 = MB * 1_024;
    if b >= GB {
        format!("{:.2} GB", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.0} MB", b as f64 / MB as f64)
    } else {
        format!("{:.0} KB", b as f64 / KB as f64)
    }
}

fn basename(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
}

pub fn render(frame: &mut Frame<'_>, area: Rect, state: &UiState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    render_swim_lanes(frame, chunks[0], state);
    render_gpu_mem_chart(frame, chunks[1], state);
}

fn render_swim_lanes(frame: &mut Frame<'_>, area: Rect, state: &UiState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Models loaded (M to toggle)");

    if state.model_loads.is_empty() {
        let widget = Paragraph::new("(no ModelLoad events yet)").block(block);
        frame.render_widget(widget, area);
        return;
    }

    // Newest-first, cap at MAX_MODEL_ROWS.
    let visible: Vec<_> = state
        .model_loads
        .iter()
        .rev()
        .take(MAX_MODEL_ROWS)
        .collect();

    let max_size = visible
        .iter()
        .map(|m| m.size_bytes)
        .max()
        .unwrap_or(1)
        .max(1);

    // Inner width available for bars (area minus 2 borders).
    let inner_width = area.width.saturating_sub(2) as usize;
    // Reserve space for label prefix and size suffix: "basename [sha8]  <bar>  size"
    // We allocate at most half the inner width to the bar.
    let bar_max = (inner_width / 2).max(4);

    let mut lines: Vec<Line> = Vec::new();
    for m in &visible {
        let key = m.sha8.as_deref().unwrap_or(m.path.as_str());
        let color = color_for(key);

        let name = basename(&m.path);
        let sha_tag = m
            .sha8
            .as_deref()
            .map(|s| format!(" [{s}]"))
            .unwrap_or_default();
        let size_label = human_bytes(m.size_bytes);

        let bar_len = ((m.size_bytes as f64 / max_size as f64) * bar_max as f64) as usize;
        let bar_len = bar_len.max(1);
        let bar: String = "█".repeat(bar_len);

        let text = format!("{name}{sha_tag}  {bar}  {size_label}");
        lines.push(Line::styled(text, Style::default().fg(color)));
    }

    let widget = Paragraph::new(lines).block(block);
    frame.render_widget(widget, area);
}

fn render_gpu_mem_chart(frame: &mut Frame<'_>, area: Rect, state: &UiState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("GPU memory (Metal device allocated)");

    if state.gpu_mem_samples.len() < 2 {
        let widget = Paragraph::new("(waiting for MetalDeviceMemSample events…)").block(block);
        frame.render_widget(widget, area);
        return;
    }

    let t0 = state.gpu_mem_samples[0].0;
    // Convert to (seconds, MB) pairs.
    let points: Vec<(f64, f64)> = state
        .gpu_mem_samples
        .iter()
        .map(|(ts, bytes)| {
            let x = ts.saturating_sub(t0) as f64 / 1e9;
            let y = *bytes as f64 / (1024.0 * 1024.0);
            (x, y)
        })
        .collect();

    let x_max = points.last().map(|(x, _)| *x).unwrap_or(1.0).max(1.0);
    let y_max = points
        .iter()
        .map(|(_, y)| *y)
        .fold(0.0_f64, f64::max)
        .max(1.0);

    let dataset = Dataset::default()
        .name("GPU mem")
        .data(&points)
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(Color::Cyan));

    let x_labels = vec![
        Line::from("0s"),
        Line::from(format!("{:.0}s", x_max / 2.0)),
        Line::from(format!("{:.0}s", x_max)),
    ];
    let y_unit = if y_max >= 1024.0 { "GB" } else { "MB" };
    let y_scale = if y_max >= 1024.0 { 1024.0 } else { 1.0 };
    let y_labels = vec![
        Line::from("0"),
        Line::from(format!("{:.1}{y_unit}", y_max / y_scale / 2.0)),
        Line::from(format!("{:.1}{y_unit}", y_max / y_scale)),
    ];

    let chart = Chart::new(vec![dataset])
        .block(block)
        .x_axis(
            Axis::default()
                .title("time (s)")
                .bounds([0.0, x_max])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .title("allocated")
                .bounds([0.0, y_max])
                .labels(y_labels),
        );

    frame.render_widget(chart, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_for_is_deterministic_per_key() {
        let c1 = color_for("deadbeef");
        let c2 = color_for("deadbeef");
        assert_eq!(c1, c2);
        // Different keys may produce different colours.
        let c3 = color_for("/models/other/model.safetensors");
        // Not asserting inequality — hash collision possible, but determinism is the key property.
        let _ = c3;
    }

    #[test]
    fn human_bytes_formats_gb_mb_kb() {
        assert_eq!(human_bytes(2_000_000_000), "1.86 GB");
        assert_eq!(human_bytes(500_000_000), "477 MB");
        assert_eq!(human_bytes(500_000), "488 KB");
    }

    #[test]
    fn basename_extracts_last_path_component() {
        assert_eq!(
            basename("/models/gemma-2b/model.safetensors"),
            "model.safetensors"
        );
        assert_eq!(basename("plain.safetensors"), "plain.safetensors");
        assert_eq!(basename("/"), "/");
    }
}

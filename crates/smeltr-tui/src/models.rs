//! TUI "Models" view — swim lanes per loaded safetensors/MLX file plus a
//! stacked per-model area and overlaid total GPU-memory line chart.

use crate::state::{ModelLoadSample, ModelUnloadSample, UiState};
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

/// Compute per-model stacked area points.
///
/// Returns `Vec<(key, color, Vec<(t_sec, cumulative_bytes_f64)>)>` sorted by
/// first-load time (oldest first). Each vector of points holds the cumulative
/// file-bytes attributed to that model key at each event timestamp.
///
/// - A ModelLoad at t adds `size_bytes` to that key's band.
/// - A ModelUnload at t subtracts `size_bytes` (clamped to 0).
///
/// Key: sha8 if present, else canonical path.
pub fn compute_stacked_points(
    loads: &[ModelLoadSample],
    unloads: &[ModelUnloadSample],
) -> Vec<(String, Color, Vec<(f64, f64)>)> {
    // Build a timeline of (t_ns, key, delta_bytes) events.
    // Sort by t_ns so bands are correct for interleaved loads/unloads.
    #[derive(Clone)]
    struct Ev {
        t_ns: u64,
        key: String,
        delta: i64, // positive = load, negative = unload
    }

    let mut events: Vec<Ev> = Vec::new();

    // Track per-key the most recent load size_bytes (for unload delta).
    // Key → last known size_bytes (from the most recent ModelLoad).
    let mut key_last_size: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();
    // Track insertion order for stable coloring.
    let mut key_order: Vec<String> = Vec::new();

    for load in loads {
        let key = load.sha8.clone().unwrap_or_else(|| load.path.clone());
        if !key_order.contains(&key) {
            key_order.push(key.clone());
        }
        key_last_size.insert(key.clone(), load.size_bytes);
        events.push(Ev {
            t_ns: load.t_start_ns,
            key,
            delta: load.size_bytes as i64,
        });
    }

    for unload in unloads {
        let key = unload.sha8.clone().unwrap_or_else(|| unload.path.clone());
        // Use the last known load size for the subtraction.
        let size = key_last_size.get(&key).copied().unwrap_or(0);
        if !key_order.contains(&key) {
            key_order.push(key.clone());
        }
        events.push(Ev {
            t_ns: unload.t_ns,
            key,
            delta: -(size as i64),
        });
    }

    events.sort_by_key(|e| e.t_ns);

    // Compute cumulative per-key values over time.
    let mut cumulative: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    // Per-key point lists.
    let mut series: std::collections::HashMap<String, Vec<(f64, f64)>> =
        std::collections::HashMap::new();

    // Global time reference: minimum t_ns across all events.
    let t0 = events.first().map(|e| e.t_ns).unwrap_or(0);

    for ev in &events {
        let cum = cumulative.entry(ev.key.clone()).or_insert(0i64);
        *cum += ev.delta;
        let val = (*cum).max(0) as f64;
        let t_sec = ev.t_ns.saturating_sub(t0) as f64 / 1e9;
        series.entry(ev.key.clone()).or_default().push((t_sec, val));
    }

    // Build output in insertion order.
    key_order
        .iter()
        .filter_map(|key| {
            let pts = series.remove(key)?;
            let color = color_for(key);
            Some((key.clone(), color, pts))
        })
        .collect()
}

fn render_gpu_mem_chart(frame: &mut Frame<'_>, area: Rect, state: &UiState) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Model file sizes + GPU memory");

    let has_gpu_mem = state.gpu_mem_samples.len() >= 2;
    let stacked = compute_stacked_points(&state.model_loads, &state.model_unloads);

    if !has_gpu_mem && stacked.is_empty() {
        let widget =
            Paragraph::new("(waiting for ModelLoad / MetalDeviceMemSample events…)").block(block);
        frame.render_widget(widget, area);
        return;
    }

    // Time reference: earliest t0 across GPU mem samples and stacked load events.
    let gpu_t0 = if has_gpu_mem {
        state.gpu_mem_samples[0].0
    } else {
        u64::MAX
    };
    let stack_t0 = state
        .model_loads
        .first()
        .map(|m| m.t_start_ns)
        .unwrap_or(u64::MAX);
    let t0 = gpu_t0.min(stack_t0);

    // GPU mem points relative to t0.
    let gpu_points: Vec<(f64, f64)> = if has_gpu_mem {
        state
            .gpu_mem_samples
            .iter()
            .map(|(ts, bytes)| {
                let x = ts.saturating_sub(t0) as f64 / 1e9;
                let y = *bytes as f64 / (1024.0 * 1024.0);
                (x, y)
            })
            .collect()
    } else {
        Vec::new()
    };

    // Stacked points re-offset to t0 (compute_stacked_points uses its own t0).
    // Re-compute relative to our global t0 for alignment.
    let stack_t0_actual = state
        .model_loads
        .iter()
        .map(|m| m.t_start_ns)
        .min()
        .unwrap_or(t0);
    let offset_sec = stack_t0_actual.saturating_sub(t0) as f64 / 1e9;

    // Shift all stacked points by the offset so they align on the same x-axis.
    let stacked_shifted: Vec<(String, Color, Vec<(f64, f64)>)> = stacked
        .into_iter()
        .map(|(key, color, pts)| {
            let shifted: Vec<(f64, f64)> =
                pts.into_iter().map(|(x, y)| (x + offset_sec, y)).collect();
            (key, color, shifted)
        })
        .collect();

    // Determine axis bounds.
    let x_max = {
        let gpu_x = gpu_points.last().map(|(x, _)| *x).unwrap_or(0.0);
        let stack_x = stacked_shifted
            .iter()
            .flat_map(|(_, _, pts)| pts.iter().map(|(x, _)| *x))
            .fold(0.0_f64, f64::max);
        gpu_x.max(stack_x).max(1.0)
    };

    let y_max = {
        let gpu_y = gpu_points.iter().map(|(_, y)| *y).fold(0.0_f64, f64::max);
        let stack_y = stacked_shifted
            .iter()
            .flat_map(|(_, _, pts)| pts.iter().map(|(_, y)| *y))
            .fold(0.0_f64, f64::max);
        gpu_y.max(stack_y).max(1.0)
    };

    // Build datasets. Stacked model bands first, GPU mem on top.
    // We need to own the point vecs for the lifetime of Dataset::data(&...).
    let mut owned_pts: Vec<Vec<(f64, f64)>> = stacked_shifted
        .iter()
        .map(|(_, _, pts)| pts.clone())
        .collect();
    owned_pts.push(gpu_points.clone());

    let mut datasets: Vec<Dataset<'_>> = stacked_shifted
        .iter()
        .enumerate()
        .map(|(i, (key, color, _))| {
            let short_key = key
                .split('/')
                .next_back()
                .unwrap_or(key.as_str())
                .chars()
                .take(12)
                .collect::<String>();
            Dataset::default()
                .name(short_key)
                .data(&owned_pts[i])
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(*color))
        })
        .collect();

    if has_gpu_mem {
        let gpu_idx = owned_pts.len() - 1;
        datasets.push(
            Dataset::default()
                .name("GPU alloc")
                .data(&owned_pts[gpu_idx])
                .marker(Marker::Braille)
                .graph_type(GraphType::Line)
                .style(Style::default().fg(Color::DarkGray)),
        );
    }

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

    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(
            Axis::default()
                .title("time (s)")
                .bounds([0.0, x_max])
                .labels(x_labels),
        )
        .y_axis(
            Axis::default()
                .title("MB")
                .bounds([0.0, y_max])
                .labels(y_labels),
        );

    frame.render_widget(chart, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ModelLoadSample, ModelUnloadSample};

    fn make_load(path: &str, sha8: &str, size_bytes: u64, t_start_ns: u64) -> ModelLoadSample {
        ModelLoadSample {
            path: path.to_string(),
            size_bytes,
            t_start_ns,
            t_end_ns: t_start_ns + 100_000_000,
            sha8: Some(sha8.to_string()),
            framework: None,
        }
    }

    fn make_unload(path: &str, sha8: &str, t_ns: u64) -> ModelUnloadSample {
        ModelUnloadSample {
            path: path.to_string(),
            t_ns,
            sha8: Some(sha8.to_string()),
        }
    }

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

    // --- compute_stacked_points tests ---

    #[test]
    fn stacked_single_load_rises_to_size_and_stays_flat() {
        let loads = vec![make_load("/m/a.bin", "aaa", 1_000_000, 1_000_000_000)];
        let unloads = vec![];
        let result = compute_stacked_points(&loads, &unloads);
        assert_eq!(result.len(), 1, "one model → one band");
        let (_, _, pts) = &result[0];
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].0, 0.0, "first point at t=0 relative");
        assert_eq!(pts[0].1, 1_000_000.0, "y = size_bytes");
    }

    #[test]
    fn stacked_load_then_unload_rises_then_falls_to_zero() {
        let loads = vec![make_load("/m/a.bin", "aaa", 2_000_000, 1_000_000_000)];
        let unloads = vec![make_unload("/m/a.bin", "aaa", 3_000_000_000)];
        let result = compute_stacked_points(&loads, &unloads);
        assert_eq!(result.len(), 1);
        let (_, _, pts) = &result[0];
        // Two points: load event and unload event.
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].1, 2_000_000.0, "after load: y = size_bytes");
        assert_eq!(pts[1].1, 0.0, "after unload: y = 0");
    }

    #[test]
    fn stacked_two_distinct_models_have_independent_bands() {
        let loads = vec![
            make_load("/m/a.bin", "aaa", 1_000_000, 1_000_000_000),
            make_load("/m/b.bin", "bbb", 3_000_000, 2_000_000_000),
        ];
        let unloads = vec![];
        let result = compute_stacked_points(&loads, &unloads);
        assert_eq!(result.len(), 2, "two models → two bands");
        // Find each band by key.
        let aaa = result.iter().find(|(k, _, _)| k == "aaa").unwrap();
        let bbb = result.iter().find(|(k, _, _)| k == "bbb").unwrap();
        // Each has exactly one point.
        assert_eq!(aaa.2.len(), 1);
        assert_eq!(bbb.2.len(), 1);
        assert_eq!(aaa.2[0].1, 1_000_000.0);
        assert_eq!(bbb.2[0].1, 3_000_000.0);
    }

    #[test]
    fn stacked_same_path_loaded_twice_without_unload_doubles_band() {
        let loads = vec![
            make_load("/m/a.bin", "aaa", 1_000_000, 1_000_000_000),
            make_load("/m/a.bin", "aaa", 1_000_000, 2_000_000_000),
        ];
        let unloads = vec![];
        let result = compute_stacked_points(&loads, &unloads);
        assert_eq!(result.len(), 1, "same key → one band");
        let (_, _, pts) = &result[0];
        assert_eq!(pts.len(), 2);
        assert_eq!(pts[0].1, 1_000_000.0, "after first load");
        assert_eq!(pts[1].1, 2_000_000.0, "after second load — doubles");
    }
}

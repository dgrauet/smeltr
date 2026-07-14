//! Ratatui rendering for the smeltr TUI.

use crate::state::UiState;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::prelude::*;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Sparkline};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Timeline = 0,
    MetalCbs = 1,
    Memory = 2,
    Mlx = 3,
    Pressure = 4,
    Notices = 5,
}

impl Panel {
    pub fn next(self) -> Self {
        match self {
            Panel::Timeline => Panel::MetalCbs,
            Panel::MetalCbs => Panel::Memory,
            Panel::Memory => Panel::Mlx,
            Panel::Mlx => Panel::Pressure,
            Panel::Pressure => Panel::Notices,
            Panel::Notices => Panel::Timeline,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RenderCtx {
    pub focus: Panel,
    pub paused: bool,
    pub mode_label: &'static str,
    pub show_hot_kernels: bool,
    pub show_models: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ReplayGauge {
    pub playing: bool,
    /// Timeline fully played: shows `■` regardless of `playing`.
    pub at_end: bool,
    pub position_ns: u64,
    pub duration_ns: u64,
}

/// App-level display overlays threaded to the renderer (kept out of the `Copy`
/// `RenderCtx`). All `None` by default.
#[derive(Debug, Default, Clone, Copy)]
pub struct RenderOverlay<'a> {
    pub status: Option<&'a str>,
    pub filter: Option<&'a str>,
    pub filtering: Option<&'a str>,
    pub replay: Option<ReplayGauge>,
}

/// Case-insensitive substring match over a notice's kind + summary.
pub fn matches_filter(entry: &crate::state::LogEntry, query: &str) -> bool {
    format!("{} {}", entry.kind, entry.summary)
        .to_lowercase()
        .contains(&query.to_lowercase())
}

/// `mm:ss` under an hour, `h:mm:ss` from one hour up (smeltr sessions
/// routinely exceed an hour — "90:00" reads as ambiguous).
fn mmss(ns: u64) -> String {
    let total = ns / 1_000_000_000;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// One-line scrub gauge for the replay title, e.g. `▶ 01:05 / 02:10 [####----]`.
pub fn format_gauge(g: ReplayGauge) -> String {
    let icon = if g.at_end {
        "■"
    } else if g.playing {
        "▶"
    } else {
        "⏸"
    };
    let bar_len = 16usize;
    let filled = if g.duration_ns == 0 {
        0
    } else {
        ((g.position_ns as f64 / g.duration_ns as f64) * bar_len as f64).round() as usize
    }
    .min(bar_len);
    let bar: String = "#".repeat(filled) + &"-".repeat(bar_len - filled);
    format!(
        "{icon} {} / {} [{bar}]",
        mmss(g.position_ns),
        mmss(g.duration_ns)
    )
}

pub fn render(frame: &mut Frame, state: &UiState, ctx: RenderCtx, overlay: RenderOverlay) {
    let area = frame.area();

    // Models view takes the entire central area (below the timeline header).
    if ctx.show_models {
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);
        render_timeline(frame, outer[0], state, ctx, overlay.status, overlay.replay);
        crate::models::render(frame, outer[1], state);
        return;
    }

    // When the optional "Hot kernels" panel is hidden, layout is identical
    // to the original 3-section grid. When visible, an extra 8-row strip is
    // inserted between the mid grid and the notices panel.
    let outer = if ctx.show_hot_kernels {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(8),
                Constraint::Length(10),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(10),
            ])
            .split(area)
    };

    render_timeline(frame, outer[0], state, ctx, overlay.status, overlay.replay);

    let mid_rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(outer[1]);
    let row1 = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(mid_rows[0]);
    let row2 = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(mid_rows[1]);

    render_metal_cbs(frame, row1[0], state, ctx);
    render_memory(frame, row1[1], state, ctx);
    render_mlx(frame, row2[0], state, ctx);
    render_pressure(frame, row2[1], state, ctx);
    if ctx.show_hot_kernels {
        render_hot_kernels(frame, outer[2], state);
        render_notices(
            frame,
            outer[3],
            state,
            ctx,
            overlay.filter,
            overlay.filtering,
        );
    } else {
        render_notices(
            frame,
            outer[2],
            state,
            ctx,
            overlay.filter,
            overlay.filtering,
        );
    }
}

fn block(title: String, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(style)
}

fn render_timeline(
    frame: &mut Frame,
    area: Rect,
    state: &UiState,
    ctx: RenderCtx,
    status: Option<&str>,
    replay: Option<ReplayGauge>,
) {
    let pause_tag = if ctx.paused { " [PAUSED]" } else { "" };
    let title = format!(
        "smeltr · {} · session: {} · events: {}{}",
        ctx.mode_label,
        state.session_short.as_deref().unwrap_or("?"),
        state.events_total,
        pause_tag,
    );
    let title = match status {
        Some(s) => format!("{title} \u{00b7} {s}"),
        None => title,
    };
    let title = match replay {
        Some(g) => format!("{title} — {}", format_gauge(g)),
        None => title,
    };
    let data: Vec<u64> = state
        .timeline_buckets
        .iter()
        .map(|(_, c)| *c as u64)
        .collect();
    let spark = Sparkline::default()
        .block(block(title, ctx.focus == Panel::Timeline))
        .data(data.as_slice());
    frame.render_widget(spark, area);
}

fn render_metal_cbs(frame: &mut Frame, area: Rect, state: &UiState, ctx: RenderCtx) {
    let mut lines: Vec<Line> = Vec::new();
    if state.metal_queues.is_empty() {
        lines.push(Line::from("(no Metal queues observed)"));
    } else {
        for (qid, q) in state.metal_queues.iter() {
            lines.push(Line::from(format!(
                "Queue#{qid}  depth={}  in-flight={}",
                q.depth,
                q.in_flight.len()
            )));
            if let Some((cb, ts)) = q.oldest_in_flight_cb {
                let age_ms = state.last_ts_mono_ns.saturating_sub(ts) / 1_000_000;
                lines.push(Line::from(format!("  oldest cb=#{cb} age={age_ms}ms")));
            }
            if let Some(err) = q.last_completed_error {
                let style = Style::default().add_modifier(Modifier::REVERSED);
                lines.push(Line::from(Span::styled(
                    format!("  last error_code={err}"),
                    style,
                )));
            }
        }
    }
    let widget =
        Paragraph::new(lines).block(block("Metal CBs".into(), ctx.focus == Panel::MetalCbs));
    frame.render_widget(widget, area);
}

fn render_memory(frame: &mut Frame, area: Rect, state: &UiState, ctx: RenderCtx) {
    let mut lines: Vec<Line> = Vec::new();
    if let Some(m) = &state.mlx_memory {
        lines.push(Line::from(format!("MLX active  {}", human(m.active_bytes))));
        lines.push(Line::from(format!("MLX peak    {}", human(m.peak_bytes))));
        lines.push(Line::from(format!("MLX cache   {}", human(m.cache_bytes))));
    } else {
        lines.push(Line::from("(no MLX memory poll yet)"));
    }
    if let Some(v) = &state.vm_sample {
        lines.push(Line::from(format!("VM wired    {}", human(v.wired_bytes))));
        lines.push(Line::from(format!(
            "VM swap     {}",
            human(v.swap_used_bytes)
        )));
        lines.push(Line::from(format!(
            "VM page-out {:.1}/s",
            v.page_outs_per_sec
        )));
    }
    let widget = Paragraph::new(lines).block(block("Memory".into(), ctx.focus == Panel::Memory));
    frame.render_widget(widget, area);
}

fn render_mlx(frame: &mut Frame, area: Rect, state: &UiState, ctx: RenderCtx) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(format!("eval-depth {}", state.mlx_eval_depth)));
    let streams: Vec<&str> = state.mlx_streams_seen.iter().map(|s| s.as_str()).collect();
    lines.push(Line::from(format!("streams    [{}]", streams.join(", "))));
    lines.push(Line::from("recent marks:"));
    for (_, label) in state.mlx_recent_marks.iter().rev().take(6) {
        lines.push(Line::from(format!("  {label}")));
    }
    let widget =
        Paragraph::new(lines).block(block("MLX (sidecar)".into(), ctx.focus == Panel::Mlx));
    frame.render_widget(widget, area);
}

fn render_pressure(frame: &mut Frame, area: Rect, state: &UiState, ctx: RenderCtx) {
    let mut lines: Vec<Line> = Vec::new();
    if state.proc_top.is_empty() {
        lines.push(Line::from("(no proc sample yet)"));
    } else {
        for p in state.proc_top.iter().take(8) {
            let flagged = is_flagged(&p.name);
            let txt = format!("{:>5.1}% {:<32} pid={}", p.cpu_pct, p.name, p.pid);
            let line = if flagged {
                Line::from(Span::styled(
                    txt,
                    Style::default().add_modifier(Modifier::REVERSED),
                ))
            } else {
                Line::from(txt)
            };
            lines.push(line);
        }
    }
    let widget = Paragraph::new(lines).block(block(
        "System pressure".into(),
        ctx.focus == Panel::Pressure,
    ));
    frame.render_widget(widget, area);
}

/// Notices panel: GPU/Metal incidents, probe health, post-mortem triggers,
/// MLX panics, and user `smeltr mark` calls. Quiet by design — when nothing
/// goes wrong this stays empty. Use `smeltr mark "msg"` to verify the live
/// pipeline.
fn render_notices(
    frame: &mut Frame,
    area: Rect,
    state: &UiState,
    ctx: RenderCtx,
    filter: Option<&str>,
    filtering: Option<&str>,
) {
    let query = filtering.filter(|b| !b.is_empty()).or(filter);
    let items: Vec<ListItem> = state
        .log_feed
        .iter()
        .filter(|e| query.is_none_or(|q| matches_filter(e, q)))
        .rev()
        .take(20)
        .map(|e| {
            let age_s = state.last_ts_mono_ns.saturating_sub(e.ts_mono_ns) as f64 / 1e9;
            ListItem::new(format!("-{:>5.1}s  {:<11} {}", age_s, e.kind, e.summary))
        })
        .collect();
    let title = match (filtering, filter) {
        (Some(buf), _) => format!("Notices · filter: {buf}_"),
        (None, Some(q)) => format!("Notices · [filter: {q}]"),
        (None, None) => "Notices (incidents · probe-health · marks)".to_string(),
    };
    let list = List::new(items).block(block(title, ctx.focus == Panel::Notices));
    frame.render_widget(list, area);
}

fn render_hot_kernels(frame: &mut Frame, area: Rect, state: &UiState) {
    let top = state.top_hot_kernels(5);
    let mut lines: Vec<Line> = Vec::new();
    if top.is_empty() {
        lines.push(Line::from("(no MetalCbOps yet)"));
    } else {
        let total_gpu: u64 = top.iter().map(|(_, g, _)| *g).sum();
        for (name, gpu_ns, count) in top {
            let pct = if total_gpu == 0 {
                0.0
            } else {
                (gpu_ns as f64 / total_gpu as f64) * 100.0
            };
            lines.push(Line::from(format!(
                "{:>5.1}%  {:>8.2}ms  ×{:<5}  {}",
                pct,
                gpu_ns as f64 / 1e6,
                count,
                name,
            )));
        }
    }
    // Always-unfocused border — this panel is a toggleable side view,
    // not part of the Tab focus cycle.
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Hot kernels (last 30s · K to toggle)");
    let widget = Paragraph::new(lines).block(block);
    frame.render_widget(widget, area);
}

fn is_flagged(name: &str) -> bool {
    const FLAGGED: &[&str] = &[
        "ReportCrash",
        "diagnosticservicesd",
        "crashanalyticsd",
        "spindump",
        "syslogd",
    ];
    FLAGGED.iter().any(|f| name.contains(f))
}

fn human(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.0} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use crate::state::LogEntry;

    fn entry(kind: &str, summary: &str) -> LogEntry {
        LogEntry {
            ts_mono_ns: 0,
            kind: kind.into(),
            summary: summary.into(),
        }
    }

    #[test]
    fn matches_filter_is_case_insensitive_over_kind_and_summary() {
        let e = entry("MetalError", "command buffer 7 failed: OOM");
        assert!(super::matches_filter(&e, "oom")); // summary, lowercased
        assert!(super::matches_filter(&e, "metalerror")); // kind, lowercased
        assert!(super::matches_filter(&e, "BUFFER 7")); // spans within summary
        assert!(!super::matches_filter(&e, "softmax")); // no match
    }

    #[test]
    fn mmss_rolls_over_to_hours() {
        assert_eq!(super::mmss(0), "00:00");
        assert_eq!(super::mmss(65_000_000_000), "01:05");
        assert_eq!(super::mmss(3_599_000_000_000), "59:59");
        assert_eq!(super::mmss(5_400_000_000_000), "1:30:00");
        assert_eq!(super::mmss(7_325_000_000_000), "2:02:05");
    }

    #[test]
    fn format_gauge_shows_stop_icon_at_end_regardless_of_playing() {
        for playing in [true, false] {
            let s = super::format_gauge(super::ReplayGauge {
                playing,
                at_end: true,
                position_ns: 130_000_000_000,
                duration_ns: 130_000_000_000,
            });
            assert!(s.contains('■'), "playing={playing}: got {s}");
            assert!(!s.contains('▶') && !s.contains('⏸'), "got {s}");
        }
    }

    #[test]
    fn format_gauge_shows_position_duration_and_state() {
        let g = super::ReplayGauge {
            playing: true,
            at_end: false,
            position_ns: 65_000_000_000,
            duration_ns: 130_000_000_000,
        };
        let s = super::format_gauge(g);
        assert!(s.contains("01:05"), "got {s}");
        assert!(s.contains("02:10"), "got {s}");
        assert!(s.contains("▶"), "got {s}");
        let p = super::format_gauge(super::ReplayGauge {
            playing: false,
            ..g
        });
        assert!(p.contains("⏸"), "got {p}");
    }

    #[test]
    fn format_gauge_empty_session() {
        let s = super::format_gauge(super::ReplayGauge {
            playing: false,
            at_end: false,
            position_ns: 0,
            duration_ns: 0,
        });
        assert!(s.contains("00:00 / 00:00"), "got {s}");
    }
}

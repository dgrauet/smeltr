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
    Log = 5,
}

impl Panel {
    pub fn next(self) -> Self {
        match self {
            Panel::Timeline => Panel::MetalCbs,
            Panel::MetalCbs => Panel::Memory,
            Panel::Memory => Panel::Mlx,
            Panel::Mlx => Panel::Pressure,
            Panel::Pressure => Panel::Log,
            Panel::Log => Panel::Timeline,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RenderCtx {
    pub focus: Panel,
    pub paused: bool,
    pub mode_label: &'static str,
}

pub fn render(frame: &mut Frame, state: &UiState, ctx: RenderCtx) {
    let area = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(10),
        ])
        .split(area);

    render_timeline(frame, outer[0], state, ctx);

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
    render_log_feed(frame, outer[2], state, ctx);
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

fn render_timeline(frame: &mut Frame, area: Rect, state: &UiState, ctx: RenderCtx) {
    let pause_tag = if ctx.paused { " [PAUSED]" } else { "" };
    let title = format!(
        "smeltr · {} · session: {} · events: {}{}",
        ctx.mode_label,
        state.session_short.as_deref().unwrap_or("?"),
        state.events_total,
        pause_tag,
    );
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

fn render_log_feed(frame: &mut Frame, area: Rect, state: &UiState, ctx: RenderCtx) {
    let items: Vec<ListItem> = state
        .log_feed
        .iter()
        .rev()
        .take(20)
        .map(|e| {
            let age_s = state.last_ts_mono_ns.saturating_sub(e.ts_mono_ns) as f64 / 1e9;
            ListItem::new(format!("-{:>5.1}s  {:<11} {}", age_s, e.kind, e.summary))
        })
        .collect();
    let list = List::new(items).block(block("Log feed".into(), ctx.focus == Panel::Log));
    frame.render_widget(list, area);
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

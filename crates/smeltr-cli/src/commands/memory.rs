//! `smeltr memory` subcommand: per-scope MTLDevice + heap memory.

use crate::session_resolver::resolve_arg;
use anyhow::Context;
use smeltr_analyzer::memory::{
    compute_heap_breakdown, compute_memory_breakdown, compute_memory_timeline, HeapMemory,
    MemTimeline, ScopeMemory,
};
use smeltr_core::reader::read_events;

pub fn run(
    session: Option<&str>,
    last: bool,
    top: usize,
    timeline: bool,
    bucket: u64,
) -> anyhow::Result<()> {
    let dir = resolve_arg(session, last)?;
    let events = read_events(&dir).context("read session events")?;
    if timeline {
        let t = compute_memory_timeline(&events, bucket);
        print!("{}", render_timeline(&t));
        return Ok(());
    }
    let scope_memory = compute_memory_breakdown(&events);
    let heap_memory = compute_heap_breakdown(&events);
    print!("{}", render(&scope_memory, &heap_memory, top));
    Ok(())
}

/// #182: time-resolved profile — per-bucket peaks and the over-budget
/// windows the aggregated percentage used to hide.
pub(crate) fn render_timeline(t: &MemTimeline) -> String {
    let gb = |b: u64| b as f64 / 1e9;
    let mut out = String::new();
    out.push_str(&format!(
        "{:<16} {:>11} {:>11} {:>13} {:>8}\n",
        "MEMORY TIMELINE", "MLX ACTIVE", "MLX CACHE", "DEVICE ALLOC", "BUDGET%"
    ));
    for b in &t.buckets {
        let pct = if b.recommended_max_bytes > 0 {
            format!(
                "{}%",
                (b.device_alloc_bytes as f64 / b.recommended_max_bytes as f64 * 100.0).round()
                    as u64
            )
        } else {
            "-".to_string()
        };
        out.push_str(&format!(
            "t+{:<5}..t+{:<6} {:>8.2} GB {:>8.2} GB {:>10.2} GB {:>8}\n",
            format!("{}s", b.t_start_s),
            format!("{}s", b.t_end_s),
            gb(b.active_bytes),
            gb(b.cache_bytes),
            gb(b.device_alloc_bytes),
            pct
        ));
    }
    out.push('\n');
    if t.windows.is_empty() {
        out.push_str("no over-budget windows (>=90% of recommended working set)\n");
    } else {
        out.push_str(&format!("{} over-budget window(s):\n", t.windows.len()));
        for w in &t.windows {
            out.push_str(&format!(
                "  t+{}s..t+{}s peak {:.2} GB ({}%) in scope `{}`\n",
                w.t_start_s,
                w.t_end_s,
                gb(w.peak_bytes),
                w.peak_pct,
                w.peak_scope
            ));
        }
    }
    out
}

pub(crate) fn render(scopes: &[ScopeMemory], heaps: &[HeapMemory], top: usize) -> String {
    let mut out = String::new();
    render_scopes(&mut out, scopes, top);
    out.push('\n');
    render_heaps(&mut out, heaps, top);
    out
}

fn render_scopes(out: &mut String, rows: &[ScopeMemory], top: usize) {
    out.push_str(&format!(
        "{:<48} {:>12} {:>12} {:>12} {:>10}\n",
        "SCOPE PEAK MEMORY", "PEAK", "AVG", "END", "SAMPLES"
    ));
    for r in rows.iter().take(top) {
        out.push_str(&format!(
            "{:<48} {:>12} {:>12} {:>12} {:>10}\n",
            truncate(&r.qualname, 48),
            fmt_bytes(r.peak_bytes),
            fmt_bytes(r.avg_bytes),
            fmt_bytes(r.end_bytes),
            r.sample_count
        ));
    }
    if rows.len() > top {
        out.push_str(&format!("(showing top {top} of {})\n", rows.len()));
    }
    if rows.is_empty() {
        out.push_str("(no scopes with memory samples)\n");
    }
}

fn render_heaps(out: &mut String, rows: &[HeapMemory], top: usize) {
    out.push_str(&format!(
        "{:<48} {:>10} {:>16}\n",
        "HEAP PEAK", "COUNT", "BYTES"
    ));
    for r in rows.iter().take(top) {
        out.push_str(&format!(
            "{:<48} {:>10} {:>16}\n",
            truncate(&r.qualname, 48),
            r.peak_heap_count,
            fmt_bytes(r.peak_heap_bytes)
        ));
    }
    if rows.len() > top {
        out.push_str(&format!("(showing top {top} of {})\n", rows.len()));
    }
    if rows.is_empty() {
        out.push_str("(no heap allocations attributed to scopes)\n");
    }
}

fn fmt_bytes(b: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if b >= GB {
        format!("{:.2} GB", b as f64 / GB as f64)
    } else if b >= MB {
        format!("{:.2} MB", b as f64 / MB as f64)
    } else if b >= KB {
        format!("{:.2} KB", b as f64 / KB as f64)
    } else {
        format!("{b} B")
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out = s.chars().take(max - 1).collect::<String>();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_empty_breakdown_shows_section_titles() {
        let s = render(&[], &[], 20);
        assert!(s.contains("SCOPE PEAK MEMORY"));
        assert!(s.contains("HEAP PEAK"));
        assert!(s.contains("no scopes with memory samples"));
        assert!(s.contains("no heap allocations"));
    }

    #[test]
    fn render_formats_bytes_as_gb_mb() {
        let scopes = vec![ScopeMemory {
            qualname: "huge".into(),
            peak_bytes: 8 * 1024 * 1024 * 1024,
            avg_bytes: 4 * 1024 * 1024,
            end_bytes: 1024,
            sample_count: 100,
        }];
        let s = render(&scopes, &[], 20);
        assert!(s.contains("8.00 GB"));
        assert!(s.contains("4.00 MB"));
        assert!(s.contains("1.00 KB"));
    }

    #[test]
    fn render_caps_to_top_n() {
        let scopes: Vec<ScopeMemory> = (0..50)
            .map(|i| ScopeMemory {
                qualname: format!("s{i}"),
                peak_bytes: 1000 + i as u64,
                avg_bytes: 500,
                end_bytes: 800,
                sample_count: 5,
            })
            .collect();
        let s = render(&scopes, &[], 5);
        assert!(s.contains("showing top 5 of 50"));
    }
}

#[cfg(test)]
mod timeline_render_tests {
    use super::*;
    use smeltr_analyzer::memory::{MemBucket, TimelineWindow};

    #[test]
    fn renders_buckets_and_windows() {
        let t = MemTimeline {
            bucket_seconds: 10,
            buckets: vec![MemBucket {
                t_start_s: 0,
                t_end_s: 10,
                active_bytes: 12_000_000_000,
                cache_bytes: 500_000_000,
                device_alloc_bytes: 13_100_000_000,
                recommended_max_bytes: 26_800_000_000,
            }],
            windows: vec![TimelineWindow {
                t_start_s: 236,
                t_end_s: 241,
                peak_bytes: 30_700_000_000,
                peak_pct: 115,
                peak_scope: "<unscoped>".into(),
            }],
        };
        let s = render_timeline(&t);
        assert!(s.contains("t+0s"), "{s}");
        assert!(s.contains("12.00 GB"), "{s}");
        assert!(s.contains("49%"), "{s}");
        assert!(s.contains("1 over-budget window(s)"), "{s}");
        assert!(s.contains("t+236s..t+241s peak 30.70 GB (115%)"), "{s}");
    }

    #[test]
    fn no_windows_message() {
        let t = MemTimeline {
            bucket_seconds: 10,
            buckets: vec![],
            windows: vec![],
        };
        assert!(render_timeline(&t).contains("no over-budget windows"));
    }
}

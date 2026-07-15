//! `smeltr memory` subcommand: per-scope MTLDevice + heap memory.

use crate::session_resolver::resolve_arg;
use anyhow::Context;
use smeltr_analyzer::memory::{
    compute_heap_breakdown, compute_memory_breakdown, HeapMemory, ScopeMemory,
};
use smeltr_core::reader::read_events;

pub fn run(session: Option<&str>, last: bool, top: usize) -> anyhow::Result<()> {
    let dir = resolve_arg(session, last)?;
    let events = read_events(&dir).context("read session events")?;
    let scope_memory = compute_memory_breakdown(&events);
    let heap_memory = compute_heap_breakdown(&events);
    print!("{}", render(&scope_memory, &heap_memory, top));
    Ok(())
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

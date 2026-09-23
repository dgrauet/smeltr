//! `smeltr memory` subcommand: per-scope MTLDevice + heap memory.

use crate::session_resolver::resolve_arg;
use anyhow::Context;
use smeltr_analyzer::footprint::ProcFootprintSummary;
use smeltr_analyzer::memory::{
    compute_memory_timeline, memory_report, HeapMemory, MemTimeline, MemoryReport, MlxAllocator,
    ScopeMemory,
};
use smeltr_core::fmt::{binary_bytes, truncate};
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
    print!("{}", render(&memory_report(&events), top));
    Ok(())
}

/// #182: time-resolved profile — per-bucket peaks and the over-budget
/// windows the aggregated percentage used to hide.
pub(crate) fn render_timeline(t: &MemTimeline) -> String {
    let gb = smeltr_core::fmt::decimal_gb;
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

/// The same sections as `get_memory_breakdown`, from the same
/// [`MemoryReport`] (#243).
pub(crate) fn render(report: &MemoryReport, top: usize) -> String {
    let mut out = String::new();
    for note in &report.notes {
        out.push_str(&format!("ℹ {note}\n\n"));
    }
    render_scopes(&mut out, &report.scope_memory, top);
    out.push('\n');
    render_heaps(&mut out, &report.heap_memory, top);
    if !report.process_footprint.is_empty() {
        out.push('\n');
        render_footprint(&mut out, &report.process_footprint, top);
    }
    if let Some(alloc) = &report.mlx_allocator {
        out.push('\n');
        render_allocator(&mut out, alloc);
    }
    out
}

/// Per-process footprint over the traced tree — the metric jetsam kills on.
fn render_footprint(out: &mut String, rows: &[ProcFootprintSummary], top: usize) {
    out.push_str(&format!(
        "{:<32} {:>8} {:>12} {:>14} {:>10}\n",
        "PROCESS FOOTPRINT", "PID", "PEAK", "LIFETIME MAX", "SAMPLES"
    ));
    for r in rows.iter().take(top) {
        let name = if r.is_traced_root {
            format!("{} (traced)", r.name)
        } else {
            r.name.clone()
        };
        out.push_str(&format!(
            "{:<32} {:>8} {:>12} {:>14} {:>10}\n",
            truncate(&name, 32),
            r.pid,
            binary_bytes(r.peak_bytes),
            binary_bytes(r.lifetime_max_bytes),
            r.sample_count
        ));
    }
    if rows.len() > top {
        out.push_str(&format!("(showing top {top} of {})\n", rows.len()));
    }
}

/// The MLX allocator's own view: its cache is memory Metal counts as
/// allocated but MLX would hand back.
fn render_allocator(out: &mut String, a: &MlxAllocator) {
    out.push_str(&format!(
        "{:<32} {:>12} {:>12} {:>12} {:>10}\n",
        "MLX ALLOCATOR", "PEAK ACTIVE", "PEAK CACHE", "END CACHE", "SAMPLES"
    ));
    out.push_str(&format!(
        "{:<32} {:>12} {:>12} {:>12} {:>10}\n",
        "",
        binary_bytes(a.peak_active_bytes),
        binary_bytes(a.peak_cache_bytes),
        binary_bytes(a.end_cache_bytes),
        a.sample_count
    ));
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
            binary_bytes(r.peak_bytes),
            binary_bytes(r.avg_bytes),
            binary_bytes(r.end_bytes),
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
            binary_bytes(r.peak_heap_bytes)
        ));
    }
    if rows.len() > top {
        out.push_str(&format!("(showing top {top} of {})\n", rows.len()));
    }
    if rows.is_empty() {
        out.push_str("(no heap allocations attributed to scopes)\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_analyzer::memory::MemoryReport;

    #[test]
    fn render_empty_breakdown_shows_section_titles() {
        let s = render(&MemoryReport::default(), 20);
        assert!(s.contains("SCOPE PEAK MEMORY"));
        assert!(s.contains("HEAP PEAK"));
        assert!(s.contains("no scopes with memory samples"));
        assert!(s.contains("no heap allocations"));
    }

    #[test]
    fn render_formats_bytes_with_binary_units() {
        let scopes = vec![ScopeMemory {
            qualname: "huge".into(),
            peak_bytes: 8 * 1024 * 1024 * 1024,
            avg_bytes: 4 * 1024 * 1024,
            end_bytes: 1024,
            sample_count: 100,
        }];
        let s = render(
            &MemoryReport {
                scope_memory: scopes,
                ..Default::default()
            },
            20,
        );
        assert!(s.contains("8.00 GiB"));
        assert!(s.contains("4.0 MiB"));
        assert!(s.contains("1 KiB"));
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
        let s = render(
            &MemoryReport {
                scope_memory: scopes,
                ..Default::default()
            },
            5,
        );
        assert!(s.contains("showing top 5 of 50"));
    }
}

#[cfg(test)]
mod report_sections_tests {
    use super::*;
    use smeltr_analyzer::footprint::ProcFootprintSummary;
    use smeltr_analyzer::memory::{MemoryReport, MlxAllocator};

    /// #243: `get_memory_breakdown` returns the process footprint (what
    /// jetsam decides on), the MLX allocator cache and why tables are empty;
    /// `smeltr memory` showed none of it.
    #[test]
    fn render_shows_footprint_allocator_and_notes() {
        let report = MemoryReport {
            scope_memory: vec![],
            heap_memory: vec![],
            process_footprint: vec![ProcFootprintSummary {
                pid: 4242,
                name: "python".into(),
                peak_bytes: 3 * 1024 * 1024 * 1024,
                lifetime_max_bytes: 4 * 1024 * 1024 * 1024,
                is_traced_root: true,
                sample_count: 7,
            }],
            mlx_allocator: Some(MlxAllocator {
                peak_active_bytes: 2 * 1024 * 1024 * 1024,
                peak_reported_bytes: 2 * 1024 * 1024 * 1024,
                peak_cache_bytes: 1024 * 1024 * 1024,
                end_cache_bytes: 512 * 1024 * 1024,
                sample_count: 9,
            }),
            notes: vec!["no Python sidecar: install it".into()],
        };
        let s = render(&report, 20);
        assert!(s.contains("PROCESS FOOTPRINT"), "{s}");
        assert!(
            s.contains("python") && s.contains("4242") && s.contains("4.00 GiB"),
            "{s}"
        );
        assert!(s.contains("MLX ALLOCATOR"), "{s}");
        assert!(s.contains("1.00 GiB") && s.contains("512.0 MiB"), "{s}");
        assert!(s.contains("no Python sidecar: install it"), "{s}");
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

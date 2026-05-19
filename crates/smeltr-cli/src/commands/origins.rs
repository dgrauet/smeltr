//! `smeltr origins` subcommand: per-(kind, file:line) GPU time attribution.

use anyhow::{anyhow, Context};
use smeltr_analyzer::dispatch_origins::{compute_dispatch_origins, DispatchOrigin};
use smeltr_core::reader::read_events;
use smeltr_mcp::types::resolve_session;

pub fn run(session: &str, top: usize) -> anyhow::Result<()> {
    let dir = resolve_session(session)
        .map_err(|e| anyhow!("could not resolve session {session:?}: {e}"))?;
    let events = read_events(&dir).context("read session events")?;
    let origins = compute_dispatch_origins(&events);
    print!("{}", render(&origins, top));
    Ok(())
}

pub(crate) fn render(rows: &[DispatchOrigin], top: usize) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<32} {:<40} {:>12} {:>12}\n",
        "KIND", "FILE:LINE", "GPU_TIME", "DISPATCHES"
    ));
    for r in rows.iter().take(top) {
        out.push_str(&format!(
            "{:<32} {:<40} {:>12} {:>12}\n",
            truncate(&r.kind, 32),
            truncate(&r.file_line, 40),
            fmt_secs(r.gpu_ns),
            r.dispatch_count
        ));
    }
    if rows.len() > top {
        out.push_str(&format!("(showing top {top} of {})\n", rows.len()));
    }
    if rows.is_empty() {
        out.push_str(
            "(no dispatch origins — was the session recorded with SMELTR_STACK_CAPTURE=1?)\n",
        );
    }
    out
}

fn fmt_secs(ns: u64) -> String {
    format!("{:.3}s", ns as f64 / 1_000_000_000.0)
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
    fn render_empty_origins_shows_placeholder() {
        let s = render(&[], 20);
        assert!(s.contains("KIND"));
        assert!(s.contains("SMELTR_STACK_CAPTURE=1"));
    }

    #[test]
    fn render_caps_to_top_n() {
        let rows: Vec<DispatchOrigin> = (0..50)
            .map(|i| DispatchOrigin {
                kind: format!("k{i}"),
                file_line: format!("f{i}:1"),
                gpu_ns: 1000 - i as u64,
                dispatch_count: 1,
            })
            .collect();
        let s = render(&rows, 5);
        assert!(s.contains("showing top 5 of 50"));
    }

    #[test]
    fn render_formats_seconds() {
        let rows = vec![DispatchOrigin {
            kind: "Matmul".into(),
            file_line: "attention.py:127".into(),
            gpu_ns: 4_310_000_000,
            dispatch_count: 1240,
        }];
        let s = render(&rows, 20);
        assert!(s.contains("Matmul"));
        assert!(s.contains("attention.py:127"));
        assert!(s.contains("4.310s"));
        assert!(s.contains("1240"));
    }
}

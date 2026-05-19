//! `smeltr compare` subcommand: scope + op-kind deltas between two sessions.

use anyhow::{anyhow, Context};
use smeltr_analyzer::diff::{
    diff_memory, diff_sessions, MemoryDelta, OpDelta, ScopeAggregate, ScopeDelta, SessionDiff,
};
use smeltr_core::reader::read_events;
use smeltr_mcp::types::resolve_session;

pub fn run(session_a: &str, session_b: &str, top: usize) -> anyhow::Result<()> {
    let dir_a = resolve_session(session_a)
        .map_err(|e| anyhow!("could not resolve session A {session_a:?}: {e}"))?;
    let dir_b = resolve_session(session_b)
        .map_err(|e| anyhow!("could not resolve session B {session_b:?}: {e}"))?;
    let a_events = read_events(&dir_a).context("read A events")?;
    let b_events = read_events(&dir_b).context("read B events")?;
    let diff = diff_sessions(&a_events, &b_events);
    let memory_deltas = diff_memory(&a_events, &b_events);
    print!("{}", render(&diff, &memory_deltas, top));
    Ok(())
}

pub(crate) fn render(diff: &SessionDiff, memory_deltas: &[MemoryDelta], top: usize) -> String {
    let mut out = String::new();
    render_scope_deltas(&mut out, &diff.scope_deltas, top);
    out.push('\n');
    render_op_deltas(&mut out, &diff.op_deltas, top);
    out.push('\n');
    render_memory_deltas(&mut out, memory_deltas, top);
    out.push('\n');
    render_only_section(&mut out, "SCOPES ONLY IN A", &diff.scopes_only_in_a, top);
    out.push('\n');
    render_only_section(&mut out, "SCOPES ONLY IN B", &diff.scopes_only_in_b, top);
    out
}

fn render_memory_deltas(out: &mut String, rows: &[MemoryDelta], top: usize) {
    out.push_str(&format!(
        "{:<48} {:>14} {:>14} {:>16}\n",
        "MEMORY DELTAS", "A_PEAK", "B_PEAK", "DELTA"
    ));
    for r in rows.iter().take(top) {
        out.push_str(&format!(
            "{:<48} {:>14} {:>14} {:>16}\n",
            truncate(&r.qualname, 48),
            fmt_bytes(r.a_peak_bytes),
            fmt_bytes(r.b_peak_bytes),
            fmt_delta_bytes(r.delta_bytes, r.delta_pct),
        ));
    }
    if rows.len() > top {
        out.push_str(&format!("(showing top {top} of {})\n", rows.len()));
    }
    if rows.is_empty() {
        out.push_str("(no memory deltas)\n");
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

fn fmt_delta_bytes(bytes: i64, pct: Option<f64>) -> String {
    let sign = if bytes < 0 {
        "-"
    } else if bytes > 0 {
        "+"
    } else {
        ""
    };
    let abs = bytes.unsigned_abs();
    let formatted = fmt_bytes(abs);
    match pct {
        Some(p) => format!("{sign}{formatted} ({p:+.1}%)"),
        None => format!("{sign}{formatted} (n/a)"),
    }
}

fn render_scope_deltas(out: &mut String, rows: &[ScopeDelta], top: usize) {
    out.push_str(&format!(
        "{:<50} {:>10} {:>10} {:>14}\n",
        "SCOPE DELTAS", "A", "B", "DELTA"
    ));
    let shown = rows.iter().take(top);
    for r in shown {
        out.push_str(&format!(
            "{:<50} {:>10} {:>10} {:>14}\n",
            truncate(&r.qualname, 50),
            fmt_secs(r.a_gpu_ns),
            fmt_secs(r.b_gpu_ns),
            fmt_delta(r.delta_ns, r.delta_pct),
        ));
    }
    if rows.len() > top {
        out.push_str(&format!("(showing top {top} of {})\n", rows.len()));
    }
    if rows.is_empty() {
        out.push_str("(no scopes present in both sessions)\n");
    }
}

fn render_op_deltas(out: &mut String, rows: &[OpDelta], top: usize) {
    out.push_str(&format!(
        "{:<50} {:>10} {:>10} {:>14}\n",
        "OP KIND DELTAS", "A", "B", "DELTA"
    ));
    let shown = rows.iter().take(top);
    for r in shown {
        out.push_str(&format!(
            "{:<50} {:>10} {:>10} {:>14}\n",
            truncate(&r.kind, 50),
            fmt_secs(r.a_gpu_ns),
            fmt_secs(r.b_gpu_ns),
            fmt_delta(r.delta_ns, r.delta_pct),
        ));
    }
    if rows.len() > top {
        out.push_str(&format!("(showing top {top} of {})\n", rows.len()));
    }
    if rows.is_empty() {
        out.push_str("(no ops in either session)\n");
    }
}

fn render_only_section(out: &mut String, title: &str, rows: &[ScopeAggregate], top: usize) {
    out.push_str(&format!("{title}\n"));
    if rows.is_empty() {
        out.push_str("(none)\n");
        return;
    }
    for r in rows.iter().take(top) {
        out.push_str(&format!(
            "  {:<48} {:>10}\n",
            truncate(&r.qualname, 48),
            fmt_secs(r.gpu_ns)
        ));
    }
    if rows.len() > top {
        out.push_str(&format!("  (showing top {top} of {})\n", rows.len()));
    }
}

fn fmt_secs(ns: u64) -> String {
    format!("{:.3}s", ns as f64 / 1_000_000_000.0)
}

fn fmt_delta(ns: i64, pct: Option<f64>) -> String {
    let sign = if ns < 0 {
        "-"
    } else if ns > 0 {
        "+"
    } else {
        ""
    };
    let secs = ns.unsigned_abs() as f64 / 1_000_000_000.0;
    match pct {
        Some(p) => format!("{sign}{secs:.3}s ({p:+.1}%)"),
        None => format!("{sign}{secs:.3}s (n/a)"),
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
#[test]
fn truncate_counts_chars_not_bytes() {
    // 50 ASCII chars (50 bytes) — fits, untouched.
    let ascii = "a".repeat(50);
    assert_eq!(truncate(&ascii, 50), ascii);
    // 49 chars including a multibyte one (>49 bytes) — must still fit.
    let multi = format!("{}é", "a".repeat(48));
    assert_eq!(truncate(&multi, 50), multi);
    // 51 chars — must truncate to 49 chars + ellipsis = 50.
    let long = "a".repeat(51);
    let out = truncate(&long, 50);
    assert_eq!(out.chars().count(), 50);
    assert!(out.ends_with('…'));
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_analyzer::diff::SessionDiff;

    fn empty_diff() -> SessionDiff {
        SessionDiff {
            scope_deltas: vec![],
            op_deltas: vec![],
            scopes_only_in_a: vec![],
            scopes_only_in_b: vec![],
        }
    }

    #[test]
    fn render_empty_diff_shows_section_titles() {
        let s = render(&empty_diff(), &[], 20);
        assert!(s.contains("SCOPE DELTAS"));
        assert!(s.contains("OP KIND DELTAS"));
        assert!(s.contains("SCOPES ONLY IN A"));
        assert!(s.contains("SCOPES ONLY IN B"));
        assert!(s.contains("no scopes present in both sessions"));
        assert!(s.contains("no ops in either session"));
    }

    #[test]
    fn render_caps_to_top_n() {
        let mut diff = empty_diff();
        diff.scope_deltas = (0..50)
            .map(|i| ScopeDelta {
                qualname: format!("s{i}"),
                a_gpu_ns: 1000,
                b_gpu_ns: 1000 + i as u64,
                delta_ns: i as i64,
                delta_pct: Some(0.1),
            })
            .collect();
        let s = render(&diff, &[], 5);
        assert!(s.contains("showing top 5 of 50"));
    }

    #[test]
    fn render_formats_seconds_and_percent() {
        let mut diff = empty_diff();
        diff.scope_deltas = vec![ScopeDelta {
            qualname: "foo".into(),
            a_gpu_ns: 2_000_000_000,
            b_gpu_ns: 1_000_000_000,
            delta_ns: -1_000_000_000,
            delta_pct: Some(-50.0),
        }];
        let s = render(&diff, &[], 20);
        assert!(s.contains("2.000s"));
        assert!(s.contains("1.000s"));
        assert!(s.contains("-1.000s"));
        assert!(s.contains("-50.0%"));
    }

    #[test]
    fn render_includes_memory_deltas() {
        let memory = vec![MemoryDelta {
            qualname: "scope".into(),
            a_peak_bytes: 2_000_000_000,
            b_peak_bytes: 1_000_000_000,
            delta_bytes: -1_000_000_000,
            delta_pct: Some(-50.0),
        }];
        let s = render(&empty_diff(), &memory, 20);
        assert!(s.contains("MEMORY DELTAS"));
        assert!(s.contains("1.86 GB"));
        assert!(s.contains("953.67 MB"));
        assert!(s.contains("-50.0%"));
    }

    #[test]
    fn render_empty_memory_shows_placeholder() {
        let s = render(&empty_diff(), &[], 20);
        assert!(s.contains("no memory deltas"));
    }
}

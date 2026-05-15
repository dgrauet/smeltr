//! `smeltr breakdown` command.

use anyhow::{anyhow, Context, Result};
use smeltr_analyzer::{compute_breakdown, render_chrome_trace, render_table, ModuleBreakdown};
use smeltr_core::reader::{list_sessions, read_events};
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn run(
    id: Option<String>,
    last: bool,
    top: usize,
    depth: u16,
    flamegraph: Option<PathBuf>,
    chrome_trace: Option<PathBuf>,
) -> Result<()> {
    let dir = resolve_session(last, id)?;
    let events =
        read_events(&dir).with_context(|| format!("reading events from {}", dir.display()))?;
    if events.is_empty() {
        println!("no events captured - was `smeltr record` used and the model exercised?");
        return Ok(());
    }

    let root = compute_breakdown(events).context("computing breakdown")?;
    println!("{}", render_table(&root, top, depth));

    if let Some(path) = flamegraph {
        write_flamegraph(&path, &root)
            .with_context(|| format!("writing flamegraph to {}", path.display()))?;
        println!("flamegraph written to {}", path.display());
    }
    if let Some(path) = chrome_trace {
        let json = render_chrome_trace(&root);
        std::fs::write(&path, json)
            .with_context(|| format!("writing chrome trace to {}", path.display()))?;
        println!("chrome trace written to {}", path.display());
    }
    Ok(())
}

fn write_flamegraph(path: &Path, root: &ModuleBreakdown) -> Result<()> {
    let mut lines: Vec<String> = Vec::new();
    fn walk(n: &ModuleBreakdown, prefix: &str, out: &mut Vec<String>) {
        let here = if prefix.is_empty() {
            n.qualname.clone()
        } else {
            format!("{prefix};{}", n.qualname)
        };
        if n.gpu_ns_self > 0 {
            out.push(format!("{here} {}", n.gpu_ns_self));
        }
        for c in &n.children {
            walk(c, &here, out);
        }
    }
    for c in &root.children {
        walk(c, "", &mut lines);
    }
    let folded = lines.join("\n");
    let mut opts = inferno::flamegraph::Options::default();
    opts.title = "smeltr inference breakdown (ns GPU self)".into();
    opts.count_name = "ns".into();
    let mut svg = std::fs::File::create(path)?;
    inferno::flamegraph::from_reader(&mut opts, folded.as_bytes(), &mut svg)
        .map_err(|e| anyhow!("inferno: {e}"))?;
    svg.flush()?;
    Ok(())
}

fn resolve_session(arg_last: bool, session_id: Option<String>) -> Result<PathBuf> {
    let sessions = list_sessions().context("listing sessions")?;
    if sessions.is_empty() {
        return Err(anyhow!("no sessions found under SMELTR_HOME"));
    }
    if let Some(id) = session_id {
        for dir in sessions.iter().rev() {
            if dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.ends_with(&id) || n.contains(&id))
                .unwrap_or(false)
            {
                return Ok(dir.clone());
            }
        }
        return Err(anyhow!("session {id} not found"));
    }
    if arg_last {
        if let Some(pm) = sessions.iter().rev().find(|d| {
            d.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("post-mortem-"))
                .unwrap_or(false)
        }) {
            return Ok(pm.clone());
        }
    }
    sessions
        .last()
        .cloned()
        .ok_or_else(|| anyhow!("no sessions found under SMELTR_HOME"))
}

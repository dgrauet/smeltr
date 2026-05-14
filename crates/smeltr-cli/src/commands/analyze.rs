//! `smeltr analyze` command.

use anyhow::{anyhow, Context, Result};
use smeltr_analyzer::analyze;
use smeltr_core::reader::{list_sessions, read_events};
use std::path::PathBuf;

pub fn run(arg_last: bool, session_id: Option<String>) -> Result<()> {
    let dir = resolve_session(arg_last, session_id)?;
    let events =
        read_events(&dir).with_context(|| format!("reading events from {}", dir.display()))?;
    let report = analyze(&events);
    println!("{}", report.render());
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
        // Prefer the most-recent post-mortem session if any; otherwise newest.
        if let Some(pm) = sessions.iter().rev().find(|d| {
            d.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("post-mortem-"))
                .unwrap_or(false)
        }) {
            return Ok(pm.clone());
        }
    }
    Ok(sessions.last().cloned().unwrap())
}

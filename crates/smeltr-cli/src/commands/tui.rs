//! `smeltr tui` and `smeltr sessions open` commands.

use anyhow::{anyhow, Context, Result};
use smeltr_core::reader::list_sessions;
use smeltr_tui::App;
use std::path::PathBuf;
use tokio::sync::mpsc;

pub async fn run_live() -> Result<()> {
    let sock = socket_path();
    let (tx, rx) = mpsc::channel(1024);
    let sock_path = sock.clone();
    let live_task = tokio::spawn(async move {
        if let Err(e) = smeltr_tui::live::spawn(&sock_path, tx).await {
            eprintln!("live adapter ended: {e}");
        }
    });
    let app = App::new("live");
    let r = app.run(rx).await;
    live_task.abort();
    r.context("tui live")
}

pub async fn run_replay(session_arg: String, speed: f64) -> Result<()> {
    let dir = resolve_session(&session_arg)?;
    let (tx, rx) = mpsc::channel(1024);
    let replay_dir = dir.clone();
    let replay_task = tokio::spawn(async move {
        if let Err(e) = smeltr_tui::replay::spawn(&replay_dir, speed, tx).await {
            eprintln!("replay adapter ended: {e}");
        }
    });
    let app = App::new("replay");
    let r = app.run(rx).await;
    replay_task.abort();
    r.context("tui replay")
}

fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("SMELTR_SOCKET") {
        return p.into();
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(base).join("smeltr.sock")
}

fn resolve_session(arg: &str) -> Result<PathBuf> {
    let sessions = list_sessions().context("listing sessions")?;
    if sessions.is_empty() {
        return Err(anyhow!("no sessions under SMELTR_HOME"));
    }
    for dir in sessions.iter().rev() {
        if dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.contains(arg))
            .unwrap_or(false)
        {
            return Ok(dir.clone());
        }
    }
    Err(anyhow!("session matching {arg:?} not found"))
}

//! `smeltr tui` and `smeltr sessions open` commands.

use anyhow::{anyhow, Context, Result};
use smeltr_tui::App;
use std::path::PathBuf;
use tokio::sync::mpsc;

pub async fn run_live() -> Result<()> {
    let sock = socket_path();
    let (tx, rx) = mpsc::channel(1024);
    let (status_tx, status_rx) =
        tokio::sync::watch::channel(smeltr_daemon::client::ConnState::Reconnecting { attempt: 0 });
    let sock_path = sock.clone();
    let live_task = tokio::spawn(async move {
        if let Err(e) = smeltr_tui::live::spawn(&sock_path, tx, status_tx).await {
            eprintln!("live adapter ended: {e}");
        }
    });
    let mut app = App::new("live");
    app.set_conn_watch(status_rx);
    let r = app.run(rx).await;
    live_task.abort();
    r.context("tui live")
}

pub async fn run_replay(session_arg: String, speed: f64) -> Result<()> {
    let dir = resolve_session(&session_arg)?;
    let scrub = smeltr_tui::replay::load(&dir, speed).context("load session for replay")?;
    // Scrub mode drives the UI from the timeline, not the channel; the channel
    // is unused here, but `_tx` is kept alive so `rx` reports Empty rather
    // than Disconnected.
    let (_tx, rx) = mpsc::channel(1);
    let mut app = App::new("replay");
    app.set_scrub(scrub);
    app.run(rx).await.context("tui replay")
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
    // Same resolution as every other session arg (dir name, short id, full
    // UUID, or SessionMetadata.name — #164).
    smeltr_mcp::types::resolve_session(arg)
        .map_err(|e| anyhow!("session matching {arg:?} not found: {e}"))
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;

    #[test]
    #[serial]
    fn resolve_session_accepts_session_name() {
        // `sessions open` must resolve names like every other session arg (#164).
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let mut meta = SessionMetadata::now_starting(SessionId::new());
        meta.name = Some("my-replay-run".into());
        let w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "test".into()).unwrap();

        let resolved = super::resolve_session("my-replay-run").unwrap();
        assert_eq!(resolved, dir);
    }
}

//! Live mode adapter: forward daemon bus events into the TUI via the shared
//! `smeltr_daemon::client` bus client.

use smeltr_core::event::Event;
use std::path::Path;
use tokio::sync::mpsc;

pub async fn spawn(sock_path: &Path, tx: mpsc::Sender<Event>) -> std::io::Result<()> {
    smeltr_daemon::client::subscribe_events(sock_path, "smeltr-tui", tx).await
}

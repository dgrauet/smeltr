//! Live mode adapter: forward daemon bus events into the TUI via the shared
//! `smeltr_daemon::client` bus client, reconnecting across daemon restarts
//! (#114) and reporting the connection state for the banner.

use smeltr_core::event::Event;
use smeltr_daemon::client::ConnState;
use std::path::Path;
use tokio::sync::{mpsc, watch};

pub async fn spawn(
    sock_path: &Path,
    tx: mpsc::Sender<Event>,
    status: watch::Sender<ConnState>,
) -> std::io::Result<()> {
    smeltr_daemon::client::subscribe_events_reconnecting(sock_path, "smeltr-tui", tx, status).await
}

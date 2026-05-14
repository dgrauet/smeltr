//! Replay mode adapter: read session from disk and pipe events to TUI channel.

use smeltr_core::event::Event;
use smeltr_replay::Replayer;
use std::path::Path;
use tokio::sync::mpsc;

pub async fn spawn(dir: &Path, speed: f64, tx: mpsc::Sender<Event>) -> std::io::Result<()> {
    let r = Replayer::from_dir(dir)?;
    r.play(speed, tx).await;
    Ok(())
}

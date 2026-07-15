//! Reusable client for the daemon event bus: connect, perform the
//! Hello/SubscribeEvents handshake, and forward each bus `Event` to `tx`
//! until the connection closes or `tx` is dropped. Shared by `smeltr tui`
//! (live mode) and `smeltr tail`.

use crate::protocol::{ClientToDaemon, DaemonToClient};
use smeltr_core::codec::write_frame;
use smeltr_core::event::Event;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

/// Connect to the daemon socket, subscribe to the event bus, and forward every
/// `Event` to `tx`. `client` is the label sent in the Hello handshake. Returns
/// when the connection closes or `tx` is dropped.
pub async fn subscribe_events(
    sock_path: &Path,
    client: &str,
    tx: mpsc::Sender<Event>,
) -> std::io::Result<()> {
    match connect_subscribed(sock_path, client).await? {
        Some(stream) => forward_events(stream, &tx).await,
        None => Ok(()),
    }
}

/// Connect + Hello/SubscribeEvents handshake. `Ok(None)` when the daemon
/// closed the connection mid-handshake.
async fn connect_subscribed(sock_path: &Path, client: &str) -> std::io::Result<Option<UnixStream>> {
    let mut stream = UnixStream::connect(sock_path).await?;

    // Hello -> Welcome
    let mut buf = Vec::new();
    write_frame(
        &mut buf,
        &ClientToDaemon::Hello {
            client: client.to_string(),
        },
    )
    .map_err(|e| std::io::Error::other(e.to_string()))?;
    stream.write_all(&buf).await?;
    if crate::server::read_msg::<DaemonToClient>(&mut stream)
        .await?
        .is_none()
    {
        return Ok(None);
    }

    // SubscribeEvents -> Ack
    let mut buf = Vec::new();
    write_frame(&mut buf, &ClientToDaemon::SubscribeEvents)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    stream.write_all(&buf).await?;
    if crate::server::read_msg::<DaemonToClient>(&mut stream)
        .await?
        .is_none()
    {
        return Ok(None);
    }
    Ok(Some(stream))
}

/// Stream EventNotification frames into `tx` until EOF, error, or `tx`
/// closed.
async fn forward_events(mut stream: UnixStream, tx: &mpsc::Sender<Event>) -> std::io::Result<()> {
    loop {
        match crate::server::read_msg::<DaemonToClient>(&mut stream).await {
            Ok(Some(DaemonToClient::EventNotification { event })) => {
                if tx.send(event).await.is_err() {
                    return Ok(());
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Connection state reported by [`subscribe_events_reconnecting`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnState {
    Connected,
    /// Lost the daemon; sleeping before retry `attempt` (1-based).
    Reconnecting {
        attempt: u32,
    },
}

/// Like [`subscribe_events`], but survives daemon restarts: on socket EOF or
/// connect failure it retries with exponential backoff (500 ms doubling,
/// capped at 5 s) and resubscribes, reporting each transition on `status`.
/// Returns only when `tx` is closed (the consumer went away). #114: a TUI
/// attached to a crashed daemon used to freeze silently forever.
pub async fn subscribe_events_reconnecting(
    sock_path: &Path,
    client: &str,
    tx: mpsc::Sender<Event>,
    status: tokio::sync::watch::Sender<ConnState>,
) -> std::io::Result<()> {
    let mut attempt: u32 = 0;
    loop {
        match connect_subscribed(sock_path, client).await {
            Ok(Some(stream)) => {
                attempt = 0;
                let _ = status.send(ConnState::Connected);
                let _ = forward_events(stream, &tx).await;
            }
            Ok(None) => {}
            Err(_) => {}
        }
        if tx.is_closed() {
            return Ok(());
        }
        attempt += 1;
        let _ = status.send(ConnState::Reconnecting { attempt });
        let backoff = Duration::from_millis((500u64 << (attempt.min(4) - 1)).min(5_000));
        tokio::time::sleep(backoff).await;
    }
}

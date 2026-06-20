//! Reusable client for the daemon event bus: connect, perform the
//! Hello/SubscribeEvents handshake, and forward each bus `Event` to `tx`
//! until the connection closes or `tx` is dropped. Shared by `smeltr tui`
//! (live mode) and `smeltr tail`.

use crate::protocol::{ClientToDaemon, DaemonToClient};
use smeltr_core::codec::write_frame;
use smeltr_core::event::Event;
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    if read_async_frame::<DaemonToClient>(&mut stream)
        .await?
        .is_none()
    {
        return Ok(());
    }

    // SubscribeEvents -> Ack
    let mut buf = Vec::new();
    write_frame(&mut buf, &ClientToDaemon::SubscribeEvents)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    stream.write_all(&buf).await?;
    if read_async_frame::<DaemonToClient>(&mut stream)
        .await?
        .is_none()
    {
        return Ok(());
    }

    // Stream EventNotification frames.
    loop {
        match read_async_frame::<DaemonToClient>(&mut stream).await {
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

async fn read_async_frame<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
) -> std::io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    let value: T = ciborium::from_reader(&body[..])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(Some(value))
}

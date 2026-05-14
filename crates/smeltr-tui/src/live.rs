//! Live mode adapter: connect to smeltrd, subscribe to bus, forward events.

use smeltr_core::codec::write_frame;
use smeltr_core::event::Event;
use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

pub async fn spawn(sock_path: &Path, tx: mpsc::Sender<Event>) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(sock_path).await?;

    // Hello → Welcome
    let mut buf = Vec::new();
    write_frame(
        &mut buf,
        &ClientToDaemon::Hello {
            client: "smeltr-tui".into(),
        },
    )
    .map_err(|e| std::io::Error::other(e.to_string()))?;
    stream.write_all(&buf).await?;
    let _welcome: DaemonToClient = match read_async_frame(&mut stream).await? {
        Some(m) => m,
        None => return Ok(()),
    };

    // SubscribeEvents → Ack
    let mut buf = Vec::new();
    write_frame(&mut buf, &ClientToDaemon::SubscribeEvents)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    stream.write_all(&buf).await?;
    let _ack: DaemonToClient = match read_async_frame(&mut stream).await? {
        Some(m) => m,
        None => return Ok(()),
    };

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
            Err(e) => {
                if e.kind() == std::io::ErrorKind::TimedOut {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    continue;
                }
                return Err(e);
            }
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

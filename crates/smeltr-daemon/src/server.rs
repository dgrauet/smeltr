//! Unix-socket server. Reads framed `ClientToDaemon` messages, writes framed
//! `DaemonToClient` responses, holds a reference to the active session and the
//! broadcast bus.

use crate::bus::Bus;
use crate::protocol::{ClientToDaemon, DaemonToClient};
use crate::sessions::ActiveSession;
use smeltr_core::reader::{find_session_dir, list_sessions, read_events, read_metadata};
use std::sync::Arc;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::net::{UnixListener, UnixStream};

pub fn socket_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("SMELTR_SOCKET") {
        return p.into();
    }
    let base = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(base).join("smeltr.sock")
}

pub struct Server {
    listener: UnixListener,
    session: Arc<ActiveSession>,
    bus: Bus,
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl Server {
    pub fn bind(
        session: Arc<ActiveSession>,
        bus: Bus,
        shutdown: tokio::sync::watch::Sender<bool>,
    ) -> std::io::Result<Self> {
        let path = socket_path();
        let _ = std::fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(&path)?;
        Ok(Self {
            listener,
            session,
            bus,
            shutdown,
        })
    }

    pub async fn run(self) -> std::io::Result<()> {
        let mut rx = self.shutdown.subscribe();
        loop {
            tokio::select! {
                accept = self.listener.accept() => {
                    let (stream, _) = accept?;
                    let session = self.session.clone();
                    let bus = self.bus.clone();
                    let shutdown_tx = self.shutdown.clone();
                    tokio::spawn(handle_connection(stream, session, bus, shutdown_tx));
                }
                _ = rx.changed() => {
                    if *rx.borrow() { break; }
                }
            }
        }
        Ok(())
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    session: Arc<ActiveSession>,
    bus: Bus,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) {
    if let Err(e) = handle_connection_inner(&mut stream, &session, &bus, &shutdown_tx).await {
        tracing::warn!(error = %e, "connection ended with error");
    }
}

async fn handle_connection_inner(
    stream: &mut UnixStream,
    session: &Arc<ActiveSession>,
    bus: &Bus,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
) -> std::io::Result<()> {
    loop {
        let msg = match read_msg::<ClientToDaemon>(stream).await? {
            Some(m) => m,
            None => return Ok(()),
        };
        let resp = handle_msg(msg, session, bus, shutdown_tx);
        write_msg(stream, &resp).await?;
    }
}

fn handle_msg(
    msg: ClientToDaemon,
    session: &Arc<ActiveSession>,
    bus: &Bus,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
) -> DaemonToClient {
    match msg {
        ClientToDaemon::Hello { client } => {
            tracing::info!(client = %client, "client connected");
            DaemonToClient::Welcome {
                daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                active_session: session.id(),
            }
        }
        ClientToDaemon::Emit {
            source,
            pid,
            payload,
        } => match session.append(source, pid, payload) {
            Ok(ev) => {
                bus.publish(ev);
                DaemonToClient::Ack
            }
            Err(e) => DaemonToClient::Error {
                message: e.to_string(),
            },
        },
        ClientToDaemon::ListSessions => match list_sessions() {
            Ok(paths) => {
                let dirs = paths
                    .iter()
                    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .collect();
                DaemonToClient::SessionList { dirs }
            }
            Err(e) => DaemonToClient::Error {
                message: e.to_string(),
            },
        },
        ClientToDaemon::GetSession { id } => match find_session_dir(id) {
            Ok(Some(dir)) => match (read_events(&dir), read_metadata(&dir)) {
                (Ok(events), Ok(metadata)) => DaemonToClient::SessionEvents { events, metadata },
                (Err(e), _) | (_, Err(e)) => DaemonToClient::Error {
                    message: e.to_string(),
                },
            },
            Ok(None) => DaemonToClient::Error {
                message: format!("session {id} not found"),
            },
            Err(e) => DaemonToClient::Error {
                message: e.to_string(),
            },
        },
        ClientToDaemon::Shutdown => {
            let _ = shutdown_tx.send(true);
            DaemonToClient::Ack
        }
    }
}

async fn read_msg<T: serde::de::DeserializeOwned>(
    stream: &mut UnixStream,
) -> std::io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > 16 * 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    let v = ciborium::from_reader(&buf[..])
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    Ok(Some(v))
}

async fn write_msg<T: serde::Serialize>(stream: &mut UnixStream, value: &T) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(256);
    ciborium::into_writer(value, &mut buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    let len = (buf.len() as u32).to_le_bytes();
    stream.write_all(&len).await?;
    stream.write_all(&buf).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Payload, Source};

    async fn connect() -> UnixStream {
        UnixStream::connect(socket_path()).await.unwrap()
    }

    fn temp_env() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", d.path());
        std::env::set_var("SMELTR_SOCKET", d.path().join("smeltr.sock"));
        d
    }

    #[tokio::test]
    async fn hello_round_trip() {
        let _home = temp_env();
        let session = Arc::new(ActiveSession::open_new().unwrap());
        let bus = Bus::new();
        let (tx, _rx) = tokio::sync::watch::channel(false);
        let server = Server::bind(session.clone(), bus, tx.clone()).unwrap();
        tokio::spawn(server.run());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut s = connect().await;
        write_msg(
            &mut s,
            &ClientToDaemon::Hello {
                client: "test".into(),
            },
        )
        .await
        .unwrap();
        let resp: DaemonToClient = read_msg(&mut s).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonToClient::Welcome { .. }));

        write_msg(
            &mut s,
            &ClientToDaemon::Emit {
                source: Source::Mark,
                pid: None,
                payload: Payload::Mark {
                    label: "from-test".into(),
                },
            },
        )
        .await
        .unwrap();
        let resp: DaemonToClient = read_msg(&mut s).await.unwrap().unwrap();
        assert!(matches!(resp, DaemonToClient::Ack));

        let _ = tx.send(true);
    }
}

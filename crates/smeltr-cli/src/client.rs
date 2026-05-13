use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};
use smeltr_daemon::server::socket_path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

pub struct Client { stream: UnixStream }

impl Client {
    pub async fn connect() -> anyhow::Result<Self> {
        let stream = UnixStream::connect(socket_path()).await
            .map_err(|e| anyhow::anyhow!("could not connect to smeltrd: {e}. Is the daemon running? Try `smeltr daemon start`."))?;
        let mut c = Self { stream };
        c.send(&ClientToDaemon::Hello { client: "smeltr-cli".into() }).await?;
        match c.recv().await? {
            DaemonToClient::Welcome { .. } => Ok(c),
            other => Err(anyhow::anyhow!("unexpected handshake: {other:?}")),
        }
    }

    pub async fn send(&mut self, m: &ClientToDaemon) -> anyhow::Result<()> {
        let mut buf = Vec::new();
        ciborium::into_writer(m, &mut buf)?;
        let len = (buf.len() as u32).to_le_bytes();
        self.stream.write_all(&len).await?;
        self.stream.write_all(&buf).await?;
        self.stream.flush().await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> anyhow::Result<DaemonToClient> {
        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf).await?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        self.stream.read_exact(&mut buf).await?;
        Ok(ciborium::from_reader(&buf[..])?)
    }

    pub async fn request(&mut self, m: ClientToDaemon) -> anyhow::Result<DaemonToClient> {
        self.send(&m).await?;
        self.recv().await
    }
}

use crate::client::Client;
use smeltr_core::event::{Payload, Source};
use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};

pub async fn run(label: String) -> anyhow::Result<()> {
    let mut c = Client::connect().await?;
    let resp = c.request(ClientToDaemon::Emit {
        source: Source::Mark,
        pid: Some(std::process::id()),
        payload: Payload::Mark { label },
    }).await?;
    match resp {
        DaemonToClient::Ack => { println!("ok"); Ok(()) }
        DaemonToClient::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

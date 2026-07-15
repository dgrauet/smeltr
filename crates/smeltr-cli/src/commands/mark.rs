use crate::client::Client;
use smeltr_core::event::{Payload, Source};
use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};

pub async fn run(label: String, session: Option<&str>) -> anyhow::Result<()> {
    // With --session, target that recording explicitly via its scope token
    // (#133); otherwise the daemon routes the marker to the newest scoped
    // session, falling back to ambient.
    let scope_token = match session {
        None => None,
        Some(s) => {
            let dir = smeltr_mcp::types::resolve_session(s)
                .map_err(|e| anyhow::anyhow!("could not resolve session {s:?}: {e}"))?;
            let meta = smeltr_core::reader::read_metadata(&dir)?;
            Some(meta.scope_token.ok_or_else(|| {
                anyhow::anyhow!("session {s:?} has no scope token (not an active recording?)")
            })?)
        }
    };
    let mut c = Client::connect().await?;
    let resp = c
        .request(ClientToDaemon::Emit {
            source: Source::Mark,
            pid: Some(std::process::id()),
            scope_token,
            payload: Payload::Mark {
                label,
                fields: Default::default(),
            },
        })
        .await?;
    match resp {
        DaemonToClient::Ack => {
            println!("ok");
            Ok(())
        }
        DaemonToClient::Error { message } => anyhow::bail!("{message}"),
        other => anyhow::bail!("unexpected response: {other:?}"),
    }
}

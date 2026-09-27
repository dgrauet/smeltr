use crate::client::Client;
use smeltr_core::event::{Payload, Source};
use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};

pub async fn run(label: String, session: Option<&str>) -> anyhow::Result<()> {
    // With --session, target that recording explicitly via its scope token
    // (#133); otherwise the daemon routes the marker to the newest scoped
    // session, falling back to ambient.
    let scope_token = session.map(scope_token_for).transpose()?;
    let mut c = Client::connect().await?;
    let resp = c
        .request(ClientToDaemon::Emit {
            source: Source::Mark,
            pid: Some(std::process::id()),
            scope_token,
            at_uptime_raw_ns: None,
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

/// The scope token of the recording `session` names.
fn scope_token_for(session: &str) -> anyhow::Result<String> {
    let dir = smeltr_core::session_resolve::resolve_session(session)
        .map_err(|e| anyhow::anyhow!("could not resolve session {session:?}: {e}"))?;
    let meta = smeltr_core::reader::read_metadata(&dir)?;
    // The daemon forgets an ended recording's token and would route the
    // marker to the newest other recording (#267).
    if meta.ended_rfc3339.is_some() {
        anyhow::bail!("session {session:?} has ended; markers only go to active recordings");
    }
    meta.scope_token.ok_or_else(|| {
        anyhow::anyhow!("session {session:?} has no scope token (not an active recording?)")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #267: marking an ended recording used to print "ok" while the daemon,
    /// no longer knowing its token, put the marker into another recording.
    #[test]
    #[serial_test::serial]
    fn marking_an_ended_recording_is_refused() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = smeltr_core::session::SessionId::new();
        let mut meta = smeltr_core::session::SessionMetadata::now_starting(id);
        meta.scope_token = Some("tok".into());
        meta.name = Some("done-run".into());
        let w = smeltr_core::writer::SessionWriter::create(meta).unwrap();
        w.finalize(Some(0), "2026-09-27T00:00:00Z".into()).unwrap();

        let err = scope_token_for("done-run").unwrap_err().to_string();
        std::env::remove_var("SMELTR_HOME");
        assert!(err.contains("ended"), "got: {err}");
    }
}

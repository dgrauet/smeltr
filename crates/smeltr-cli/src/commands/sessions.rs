use crate::client::Client;
use clap::Subcommand;
use smeltr_core::reader::{list_sessions, read_events, read_metadata};
use smeltr_core::session::SessionId;
use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};

#[derive(Subcommand, Debug)]
pub enum SessionsCmd {
    /// List sessions on disk.
    Ls,
    /// Show summary + events of a session. Pass an 8-char short id or a full UUID.
    Show { id: String },
}

pub async fn run(cmd: SessionsCmd) -> anyhow::Result<()> {
    match cmd {
        SessionsCmd::Ls => ls().await,
        SessionsCmd::Show { id } => show(&id).await,
    }
}

async fn ls() -> anyhow::Result<()> {
    // Prefer asking the daemon (so its active session shows up even before
    // the session is finalized to disk). Fall back to direct disk read.
    let from_daemon = match Client::connect().await {
        Ok(mut c) => match c.request(ClientToDaemon::ListSessions).await {
            Ok(DaemonToClient::SessionList { dirs }) => Some(dirs),
            _ => None,
        },
        Err(_) => None,
    };
    let dirs = match from_daemon {
        Some(d) => d,
        None => list_sessions()?
            .into_iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
            .collect(),
    };
    if dirs.is_empty() {
        println!("(no sessions)");
        return Ok(());
    }
    for d in dirs {
        println!("{d}");
    }
    Ok(())
}

async fn show(id: &str) -> anyhow::Result<()> {
    let sid = resolve_id(id)?;
    // Try the daemon first.
    if let Ok(mut c) = Client::connect().await {
        match c.request(ClientToDaemon::GetSession { id: sid }).await {
            Ok(DaemonToClient::SessionEvents { events, metadata }) => {
                return print_session(&metadata, &events);
            }
            Ok(DaemonToClient::Error { message }) => {
                tracing::debug!("daemon: {message}, falling back to disk");
            }
            _ => {}
        }
    }
    let dir = smeltr_core::reader::find_session_dir(sid)?
        .ok_or_else(|| anyhow::anyhow!("session {id} not found"))?;
    let metadata = read_metadata(&dir)?;
    let events = read_events(&dir)?;
    print_session(&metadata, &events)
}

fn resolve_id(s: &str) -> anyhow::Result<SessionId> {
    if let Ok(sid) = s.parse::<SessionId>() {
        return Ok(sid);
    }
    // Allow 8-char short id by listing and matching.
    let s = s.to_lowercase();
    for p in list_sessions()? {
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.ends_with(&s) {
            let meta = read_metadata(&p)?;
            return Ok(meta.session_id);
        }
    }
    anyhow::bail!("could not resolve session id `{s}`")
}

fn print_session(
    meta: &smeltr_core::session::SessionMetadata,
    events: &[smeltr_core::event::Event],
) -> anyhow::Result<()> {
    println!("session    {}", meta.session_id);
    println!("started    {}", meta.started_rfc3339);
    if let Some(end) = &meta.ended_rfc3339 {
        println!("ended      {end}");
    }
    println!("host       {}", meta.host);
    if let Some(c) = meta.exit_code {
        println!("exit_code  {c}");
    }
    println!("events     {}", events.len());
    println!();
    for ev in events {
        let kind = match &ev.payload {
            smeltr_core::event::Payload::Mark { label } => format!("mark    {label}"),
            smeltr_core::event::Payload::SessionStarted { .. } => "session-started".into(),
            smeltr_core::event::Payload::SessionEnded { reason, .. } => {
                format!("session-ended ({reason})")
            }
        };
        println!(
            "  +{:>10}ns  seq={:>4}  src={:?}  {kind}",
            ev.ts_mono_ns, ev.seq, ev.source
        );
    }
    Ok(())
}

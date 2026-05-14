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
    /// Open a session in the TUI replay mode.
    Open {
        id: String,
        /// Playback speed multiplier (1.0 = real time, 0.0 = as fast as possible).
        #[arg(long, default_value_t = 1.0)]
        speed: f64,
    },
}

pub async fn run(cmd: SessionsCmd) -> anyhow::Result<()> {
    match cmd {
        SessionsCmd::Ls => ls().await,
        SessionsCmd::Show { id } => show(&id).await,
        SessionsCmd::Open { id, speed } => crate::commands::tui::run_replay(id, speed).await,
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
            smeltr_core::event::Payload::PythonSidecarHello {
                python_version,
                mlx_version,
                argv,
            } => {
                let mlx = mlx_version.as_deref().unwrap_or("none");
                format!("PythonSidecarHello python={python_version} mlx={mlx} argv={argv:?}")
            }
            smeltr_core::event::Payload::MetalCbCommitted {
                cb_id,
                queue_id,
                queue_depth,
                label,
            } => {
                format!(
                    "MetalCbCommitted cb_id=0x{cb_id:x} queue_id={queue_id} queue_depth={queue_depth} label={}",
                    label.as_deref().unwrap_or("-")
                )
            }
            smeltr_core::event::Payload::MetalCbScheduled { cb_id, queue_id } => {
                format!("MetalCbScheduled cb_id=0x{cb_id:x} queue_id={queue_id}")
            }
            smeltr_core::event::Payload::MetalCbCompleted {
                cb_id,
                queue_id,
                status,
                error_code,
                error_domain,
                in_flight_ns,
            } => {
                format!(
                    "MetalCbCompleted cb_id=0x{cb_id:x} queue_id={queue_id} status={status} error_code={} domain={} in_flight={}ms",
                    error_code.map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
                    error_domain.as_deref().unwrap_or("-"),
                    in_flight_ns / 1_000_000
                )
            }
            smeltr_core::event::Payload::MetalCbWarning {
                cb_id,
                queue_id,
                elapsed_ns,
            } => {
                format!(
                    "MetalCbWarning cb_id=0x{cb_id:x} queue_id={queue_id} elapsed={}ms",
                    elapsed_ns / 1_000_000
                )
            }
            smeltr_core::event::Payload::MetalHeapAlloc {
                heap_id,
                size_bytes,
                label,
            } => {
                format!(
                    "MetalHeapAlloc heap_id=0x{heap_id:x} size={} label={}",
                    human_bytes(*size_bytes),
                    label.as_deref().unwrap_or("-")
                )
            }
            smeltr_core::event::Payload::MetalHeapFree { heap_id } => {
                format!("MetalHeapFree heap_id=0x{heap_id:x}")
            }
            smeltr_core::event::Payload::MetalBufferAlloc {
                buffer_id,
                heap_id,
                size_bytes,
                label,
            } => {
                format!(
                    "MetalBufferAlloc buf=0x{buffer_id:x} heap={} size={} label={}",
                    heap_id
                        .map(|h| format!("0x{h:x}"))
                        .unwrap_or_else(|| "-".into()),
                    human_bytes(*size_bytes),
                    label.as_deref().unwrap_or("-")
                )
            }
            smeltr_core::event::Payload::MetalBufferFree { buffer_id } => {
                format!("MetalBufferFree buf=0x{buffer_id:x}")
            }
            smeltr_core::event::Payload::MetalTextureAlloc {
                texture_id,
                heap_id,
                size_bytes,
                label,
            } => {
                format!(
                    "MetalTextureAlloc tex=0x{texture_id:x} heap={} size={} label={}",
                    heap_id
                        .map(|h| format!("0x{h:x}"))
                        .unwrap_or_else(|| "-".into()),
                    human_bytes(*size_bytes),
                    label.as_deref().unwrap_or("-")
                )
            }
            smeltr_core::event::Payload::MetalTextureFree { texture_id } => {
                format!("MetalTextureFree tex=0x{texture_id:x}")
            }
            smeltr_core::event::Payload::MetalHookDropped { count } => {
                format!("MetalHookDropped count={count}")
            }
            smeltr_core::event::Payload::MetalHookSkipped { reason } => {
                format!("MetalHookSkipped reason={reason}")
            }
            smeltr_core::event::Payload::MlxEvalEntered {
                call_id,
                array_count,
                stream,
            } => {
                format!("MlxEvalEntered call_id={call_id} arrays={array_count} stream={stream}")
            }
            smeltr_core::event::Payload::MlxEvalReturned {
                call_id,
                duration_ns,
                was_async,
            } => {
                format!(
                    "MlxEvalReturned call_id={call_id} duration={}ms async={was_async}",
                    duration_ns / 1_000_000
                )
            }
            smeltr_core::event::Payload::MlxMemoryPoll {
                active_bytes,
                peak_bytes,
                cache_bytes,
            } => {
                format!(
                    "MlxMemoryPoll active={} peak={} cache={}",
                    human_bytes(*active_bytes),
                    human_bytes(*peak_bytes),
                    human_bytes(*cache_bytes)
                )
            }
            smeltr_core::event::Payload::MlxArrayAlive {
                array_id,
                size_bytes,
                dtype,
                shape,
                stream,
            } => {
                format!(
                    "MlxArrayAlive id=0x{array_id:x} size={} dtype={dtype} shape={shape:?} stream={stream}",
                    human_bytes(*size_bytes)
                )
            }
            smeltr_core::event::Payload::MlxArrayFreed { array_id } => {
                format!("MlxArrayFreed id=0x{array_id:x}")
            }
            smeltr_core::event::Payload::MlxSnapshot {
                live_arrays,
                total_array_bytes,
                streams,
                mlx_version,
            } => {
                format!(
                    "MlxSnapshot arrays={live_arrays} total={} streams={streams:?} mlx={}",
                    human_bytes(*total_array_bytes),
                    mlx_version.as_deref().unwrap_or("none")
                )
            }
            smeltr_core::event::Payload::MlxPanicTriggered { condition } => {
                format!("MlxPanicTriggered condition={condition}")
            }
            smeltr_core::event::Payload::PostMortemFlushed {
                reason,
                source_session,
                event_count,
            } => {
                format!(
                    "PostMortemFlushed reason={reason} src={source_session} events={event_count}"
                )
            }
            other => format!("{other:?}"),
        };
        println!(
            "  +{:>10}ns  seq={:>4}  src={:?}  {kind}",
            ev.ts_mono_ns, ev.seq, ev.source
        );
    }
    Ok(())
}

fn human_bytes(b: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if b >= GIB {
        format!("{:.2} GiB", b as f64 / GIB as f64)
    } else if b >= MIB {
        format!("{:.1} MiB", b as f64 / MIB as f64)
    } else if b >= KIB {
        format!("{:.0} KiB", b as f64 / KIB as f64)
    } else {
        format!("{b} B")
    }
}

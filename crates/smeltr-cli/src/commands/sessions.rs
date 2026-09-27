use clap::Subcommand;
use smeltr_core::fmt::binary_bytes;
use smeltr_core::reader::{read_events, read_metadata};

#[derive(Subcommand, Debug)]
pub enum SessionsCmd {
    /// List sessions on disk.
    Ls,
    /// Show a session's metadata, then every event. Session reference: short id,
    /// full UUID, directory name or name.
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
    let dirs = ls_order()?;
    if dirs.is_empty() {
        println!("(no sessions)");
        return Ok(());
    }
    let root = smeltr_core::session::sessions_root();
    for d in &dirs {
        let dir = root.join(d);
        let meta = read_metadata(&dir).ok();
        let kind_label = match &meta {
            Some(m) => match &m.kind {
                smeltr_core::session::SessionKind::Ambient => "ambient".to_string(),
                smeltr_core::session::SessionKind::Scoped { pid, argv } => {
                    let cmd = argv.first().map(|s| s.as_str()).unwrap_or("?");
                    format!("scoped pid={pid} cmd={cmd}")
                }
            },
            None => "?".to_string(),
        };
        let name_suffix = match &meta {
            Some(m) => match m.name.as_deref() {
                Some(n) => format!("  name=\"{n}\""),
                None => String::new(),
            },
            None => String::new(),
        };
        println!("{d}  [{kind_label}]{name_suffix}");
    }
    Ok(())
}

async fn show(id: &str) -> anyhow::Result<()> {
    // Straight from disk (#267): the daemon's GetSession sent the whole
    // session in one frame, far past what a client reads for large ones,
    // and the daemon flushes live sessions to disk every 500 ms anyway.
    let (metadata, events) = load_session(id)?;
    print_session(&metadata, &events)
}

type LoadedSession = (
    smeltr_core::session::SessionMetadata,
    Vec<smeltr_core::event::Event>,
);

/// Session directory names, oldest to newest by start time (#270): sorted by
/// directory name, every `post-mortem-*` listed after every dated session.
/// Read from disk — the daemon's list came from the same directory, and an
/// active session has its directory from the moment it opens.
fn ls_order() -> anyhow::Result<Vec<String>> {
    let mut dirs = smeltr_core::session_resolve::sessions_newest_first()?;
    dirs.reverse();
    Ok(dirs
        .into_iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect())
}

/// Read a session from disk, resolved like every other session argument.
fn load_session(id: &str) -> anyhow::Result<LoadedSession> {
    let dir = smeltr_core::session_resolve::resolve_session(id)
        .map_err(|e| anyhow::anyhow!("could not resolve session `{id}`: {e}"))?;
    Ok((read_metadata(&dir)?, read_events(&dir)?))
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
            smeltr_core::event::Payload::Mark { label, .. } => format!("mark    {label}"),
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
                    binary_bytes(*size_bytes),
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
                    binary_bytes(*size_bytes),
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
                    binary_bytes(*size_bytes),
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
                ..
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
                    binary_bytes(*active_bytes),
                    binary_bytes(*peak_bytes),
                    binary_bytes(*cache_bytes)
                )
            }
            smeltr_core::event::Payload::ProcFootprint {
                pid,
                name,
                phys_footprint_bytes,
                lifetime_max_phys_footprint_bytes,
                ..
            } => format!(
                "ProcFootprint pid={pid} {name} phys={phys_footprint_bytes} \
                 lifetime_max={lifetime_max_phys_footprint_bytes}"
            ),
            smeltr_core::event::Payload::JetsamKill {
                killed_pid,
                killed_name,
                footprint_bytes,
                lifetime_max_bytes,
                reason,
                ..
            } => format!(
                "JetsamKill pid={} {killed_name} reason={} footprint={} \
                 lifetime_max={}",
                killed_pid.map_or_else(|| "-".to_string(), |p| p.to_string()),
                reason.as_deref().unwrap_or("-"),
                binary_bytes(*footprint_bytes),
                binary_bytes(*lifetime_max_bytes)
            ),
            smeltr_core::event::Payload::MlxArrayAlive {
                array_id,
                size_bytes,
                dtype,
                shape,
                stream,
            } => {
                format!(
                    "MlxArrayAlive id=0x{array_id:x} size={} dtype={dtype} shape={shape:?} stream={stream}",
                    binary_bytes(*size_bytes)
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
                    binary_bytes(*total_array_bytes),
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

#[cfg(test)]
mod tests {
    /// #270: a post-mortem written before a recording used to list after it.
    #[test]
    #[serial]
    fn ls_lists_by_start_time() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let pm = home
            .path()
            .join("sessions/post-mortem-crash-report-2026-09-01-000000-aaaa1111");
        std::fs::create_dir_all(&pm).unwrap();
        let mut meta = SessionMetadata::now_starting(SessionId::new());
        meta.started_rfc3339 = "2026-09-01T00:00:00Z".into();
        smeltr_core::session::write_metadata(&pm, &meta).unwrap();
        let mut later = SessionMetadata::now_starting(SessionId::new());
        later.started_rfc3339 = "2026-09-02T00:00:00Z".into();
        let later_dir = SessionWriter::create(later).unwrap().dir().to_path_buf();
        let got = super::ls_order().unwrap();
        std::env::remove_var("SMELTR_HOME");
        assert_eq!(
            got,
            vec![
                "post-mortem-crash-report-2026-09-01-000000-aaaa1111".to_string(),
                later_dir
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            ]
        );
    }

    /// #267: `sessions show` went through the daemon's GetSession (one
    /// unbounded frame: 116 MB for a large ambient session), and resolved
    /// the argument to an id, then back to a directory by id suffix — so a
    /// copied or renamed session directory was "not found" while every
    /// other command read it.
    #[test]
    #[serial_test::serial]
    fn show_reads_a_renamed_session_directory_from_disk() {
        use super::load_session;
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = smeltr_core::session::SessionId::new();
        let meta = smeltr_core::session::SessionMetadata::now_starting(id);
        let w = smeltr_core::writer::SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "2026-09-27T00:00:00Z".into()).unwrap();
        let renamed = dir.with_file_name("my-copy");
        std::fs::rename(&dir, &renamed).unwrap();

        let loaded = load_session("my-copy");
        std::env::remove_var("SMELTR_HOME");
        let (meta, _events) = loaded.unwrap();
        assert_eq!(meta.session_id, id);
    }

    use serial_test::serial;
    use smeltr_core::session::{SessionId, SessionKind, SessionMetadata};
    use smeltr_core::writer::SessionWriter;

    #[test]
    #[serial]
    fn show_accepts_session_name() {
        // `sessions show` must resolve names like every other subcommand
        // (analyze/breakdown/origins go through resolve_session — #164).
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.name = Some("my-named-run".into());
        let w = SessionWriter::create(meta).unwrap();
        w.finalize(Some(0), "test".into()).unwrap();

        let (meta, _) = super::load_session("my-named-run").unwrap();
        assert_eq!(meta.session_id, id);
    }

    #[test]
    #[serial]
    fn metadata_kind_renders_distinct_labels() {
        // Direct unit on the formatting logic — no daemon, no socket.
        // We just exercise the match on SessionKind that ls() uses.
        let amb = SessionKind::Ambient;
        let sc = SessionKind::Scoped {
            pid: 4242,
            argv: vec!["/bin/sleep".into(), "1".into()],
        };
        let render = |k: &SessionKind| match k {
            SessionKind::Ambient => "ambient".to_string(),
            SessionKind::Scoped { pid, argv } => {
                let cmd = argv.first().map(|s| s.as_str()).unwrap_or("?");
                format!("scoped pid={pid} cmd={cmd}")
            }
        };
        assert_eq!(render(&amb), "ambient");
        assert_eq!(render(&sc), "scoped pid=4242 cmd=/bin/sleep");
    }

    #[test]
    #[serial]
    fn metadata_persisted_kind_round_trip() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let mut meta = SessionMetadata::now_starting(id);
        meta.kind = SessionKind::Scoped {
            pid: 999,
            argv: vec!["python".into(), "x.py".into()],
        };
        let w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        w.finalize(Some(0), "test".into()).unwrap();
        let parsed = smeltr_core::reader::read_metadata(&dir).unwrap();
        match parsed.kind {
            SessionKind::Scoped { pid, argv } => {
                assert_eq!(pid, 999);
                assert_eq!(argv[0], "python");
            }
            _ => panic!("expected Scoped"),
        }
    }
}

use clap::Parser;
use smeltr_daemon::{bus, probes, server, session_router, sessions};
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "smeltrd", version)]
struct Args {
    /// Run in foreground (default). The CLI launches us via tokio.
    #[arg(long, default_value_t = false)]
    foreground: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let _args = Args::parse();

    let flight_recorder = Arc::new(smeltr_daemon::flight_recorder::FlightRecorder::new(
        std::time::Duration::from_secs(60),
    ));
    let bus = bus::Bus::new();
    let ambient = Arc::new(sessions::ActiveSession::open_new_full(
        Some(flight_recorder.clone()),
        Some(bus.clone()),
    )?);
    tracing::info!(session = %ambient.id(), "active session opened");
    let router = Arc::new(session_router::SessionRouter::new(
        ambient.clone(),
        Some(flight_recorder.clone()),
        Some(bus.clone()),
    ));
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    // Write PID file for `smeltr daemon stop`.
    let pid_path = pid_file_path();
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&pid_path, std::process::id().to_string())?;

    // Periodic flush so external readers see in-flight events.
    let flush_router = router.clone();
    let mut flush_shutdown = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(e) = flush_router.flush_all() {
                        tracing::warn!(error = %e, "periodic session flush failed");
                    }
                }
                _ = flush_shutdown.changed() => {
                    if *flush_shutdown.borrow() { break; }
                }
            }
        }
    });

    // Post-mortem trigger watcher: subscribe to bus, flush flight recorder on
    // crash-like events, and emit a PostMortemFlushed event back into the
    // ambient session.
    let trigger_session = ambient.clone();
    let trigger_fr = flight_recorder.clone();
    let mut bus_rx = bus.subscribe();
    let mut trigger_shutdown = shutdown_tx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                msg = bus_rx.recv() => {
                    match msg {
                        Ok(ev) => {
                            if let Some(reason) = smeltr_daemon::triggers::classify(&ev) {
                                tracing::warn!(reason = ?reason, "post-mortem trigger fired");
                                match smeltr_daemon::triggers::flush_post_mortem(&trigger_fr, &reason) {
                                    Ok(summary) => {
                                        tracing::info!(
                                            dir = ?summary.session_dir,
                                            count = summary.event_count,
                                            "post-mortem session written"
                                        );
                                        let short = summary.session_dir.file_name()
                                            .and_then(|n| n.to_str())
                                            .unwrap_or("unknown")
                                            .to_string();
                                        let _ = trigger_session.append(
                                            smeltr_core::event::Source::System,
                                            None,
                                            smeltr_core::event::Payload::PostMortemFlushed {
                                                reason: reason.label(),
                                                source_session: short,
                                                event_count: summary.event_count as u32,
                                            },
                                        );
                                    }
                                    Err(e) => tracing::error!(error = %e, "post-mortem flush failed"),
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "trigger watcher lagged behind bus");
                        }
                        Err(_) => break,
                    }
                }
                _ = trigger_shutdown.changed() => {
                    if *trigger_shutdown.borrow() { break; }
                }
            }
        }
    });

    let sink = Arc::new(probes::DaemonSink {
        router: router.clone(),
    });
    let probe_runtime = Arc::new(probes::ProbeRuntime::start_global(sink));

    let server = server::Server::bind(
        router.clone(),
        bus.clone(),
        probe_runtime.clone(),
        shutdown_tx.clone(),
    )?;
    let server_task = tokio::spawn(server.run());

    // SIGTERM / SIGINT → graceful shutdown
    let shutdown_signal = shutdown_tx.clone();
    tokio::spawn(async move {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        let mut sigint =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).unwrap();
        tokio::select! {
            _ = sigterm.recv() => tracing::info!("SIGTERM, shutting down"),
            _ = sigint.recv()  => tracing::info!("SIGINT, shutting down"),
        }
        let _ = shutdown_signal.send(true);
    });

    while !*shutdown_rx.borrow() {
        shutdown_rx.changed().await?;
    }

    // Drop server task (listener will close)
    server_task.abort();

    probe_runtime.shutdown().await;

    // Finalize all sessions (ambient + any remaining scoped) on graceful shutdown.
    if let Err(e) = router.finalize_all("daemon shutdown") {
        tracing::error!(error = %e, "failed to finalize sessions on shutdown");
    }

    let _ = std::fs::remove_file(&pid_path);
    let _ = std::fs::remove_file(server::socket_path());
    Ok(())
}

fn pid_file_path() -> std::path::PathBuf {
    let base = std::env::var("SMELTR_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var_os("HOME").expect("HOME must be set");
            std::path::PathBuf::from(home).join(".smeltr")
        });
    base.join("smeltrd.pid")
}

use clap::Parser;
use smeltr_daemon::{bus, probes, server, sessions};
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

    let session = Arc::new(sessions::ActiveSession::open_new()?);
    tracing::info!(session = %session.id(), "active session opened");
    let bus = bus::Bus::new();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    // Write PID file for `smeltr daemon stop`.
    let pid_path = pid_file_path();
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&pid_path, std::process::id().to_string())?;

    let sink = Arc::new(probes::DaemonSink {
        session: session.clone(),
        bus: bus.clone(),
    });
    let probe_runtime = Arc::new(probes::ProbeRuntime::start_global(sink));

    let server = server::Server::bind(
        session.clone(),
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

    // Finalize session (idempotent, safe with outstanding Arc clones).
    if let Err(e) = session.finalize(Some(0), "daemon shutdown") {
        tracing::error!(error = %e, "failed to finalize session on shutdown");
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

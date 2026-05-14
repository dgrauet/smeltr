mod client;
mod commands;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "smeltr", version)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Manage the smeltrd daemon.
    Daemon {
        #[command(subcommand)]
        sub: commands::daemon::DaemonCmd,
    },
    /// Append a marker event to the active session.
    Mark { label: String },
    /// Inspect sessions on disk.
    Sessions {
        #[command(subcommand)]
        sub: commands::sessions::SessionsCmd,
    },
    /// Audit probe availability and permissions.
    Doctor,
    /// Live TUI: connect to the running daemon and stream events.
    Tui,
    /// Analyze a session and print the contributing-factor report.
    Analyze {
        /// Use the most recent post-mortem session (or newest if none).
        #[arg(long)]
        last: bool,
        /// Session id or directory-name suffix to analyze.
        id: Option<String>,
    },
    /// Run the MCP stdio server (used by LLM clients, e.g. Claude Desktop).
    Mcp,
    /// Spawn a child process under smeltr's scoped probes.
    Record {
        /// Command to execute.
        cmd: String,
        /// Arguments to pass to the command.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Skip the Metal hook (no DYLD_INSERT_LIBRARIES).
        #[arg(long)]
        no_hook: bool,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();
    let args = Args::parse();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        match args.cmd {
            Cmd::Daemon { sub } => commands::daemon::run(sub).await,
            Cmd::Mark { label } => commands::mark::run(label).await,
            Cmd::Sessions { sub } => commands::sessions::run(sub).await,
            Cmd::Doctor => commands::doctor::run(),
            Cmd::Tui => commands::tui::run_live().await,
            Cmd::Analyze { last, id } => commands::analyze::run(last, id),
            Cmd::Mcp => commands::mcp::run().await,
            Cmd::Record { cmd, args, no_hook } => {
                let code = commands::record::run(&cmd, &args, no_hook).await?;
                std::process::exit(code);
            }
        }
    })
}

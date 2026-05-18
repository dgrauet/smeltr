mod client;
mod commands;
mod session_resolver;

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
        /// Include the daemon's ambient session when picking --last.
        #[arg(long)]
        include_ambient: bool,
    },
    /// Per-module GPU time breakdown for an MLX inference session.
    Breakdown {
        /// Use the most recent session.
        #[arg(long)]
        last: bool,
        /// Session id or directory-name suffix.
        id: Option<String>,
        /// Include the daemon's ambient session when picking --last.
        #[arg(long)]
        include_ambient: bool,
        /// Max rows in the table output.
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Max tree depth in the table output.
        #[arg(long, default_value_t = 6)]
        depth: u16,
        /// Write a folded-stack flamegraph SVG here.
        #[arg(long)]
        flamegraph: Option<std::path::PathBuf>,
        /// Write a Chrome Trace Event Format JSON here.
        #[arg(long)]
        chrome_trace: Option<std::path::PathBuf>,
        /// Max ops shown per module leaf.
        #[arg(long, default_value_t = 5)]
        top_ops: usize,
        /// Hide per-op breakdown under each leaf.
        #[arg(long, default_value_t = false)]
        no_ops: bool,
        /// Print a flat cross-module ops table instead of the module tree.
        #[arg(long, default_value_t = false)]
        ops_flat: bool,
    },
    /// Export a recorded session to chrome-trace JSON (openable in
    /// chrome://tracing / Perfetto / Speedscope) or raw JSON.
    Export {
        /// Session reference: 8-char short id, full UUID, or
        /// SessionMetadata.name. Same resolution rules as other smeltr
        /// commands (most-recent-wins on name collision).
        session: String,
        /// Output format. Default: `chrome-trace`.
        #[arg(long, default_value = "chrome-trace")]
        format: String,
        /// Output path. Use `-` for stdout. Default: `<short_id>.json`.
        #[arg(long, short = 'o')]
        output: Option<String>,
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
        /// Human-readable name for the recorded session. Sets
        /// SMELTR_SESSION_NAME in the child process environment. The
        /// session metadata records the name and `list_sessions`
        /// surfaces it. Overrides any inherited SMELTR_SESSION_NAME.
        #[arg(long)]
        name: Option<String>,
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
            Cmd::Analyze {
                last,
                id,
                include_ambient,
            } => commands::analyze::run(last, id, include_ambient),
            Cmd::Breakdown {
                last,
                id,
                include_ambient,
                top,
                depth,
                flamegraph,
                chrome_trace,
                top_ops,
                no_ops,
                ops_flat,
            } => commands::breakdown::run(
                id,
                last,
                include_ambient,
                top,
                depth,
                flamegraph,
                chrome_trace,
                top_ops,
                no_ops,
                ops_flat,
            ),
            Cmd::Export {
                session,
                format,
                output,
            } => commands::export::run(&session, &format, output.as_deref()),
            Cmd::Mcp => commands::mcp::run().await,
            Cmd::Record {
                cmd,
                args,
                no_hook,
                name,
            } => {
                let code = commands::record::run(&cmd, &args, no_hook, name.as_deref()).await?;
                std::process::exit(code);
            }
        }
    })
}

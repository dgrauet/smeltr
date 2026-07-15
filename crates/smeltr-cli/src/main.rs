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
    Mark {
        label: String,
        /// Target a specific recording (short id, full UUID, or name).
        /// Default: the newest active recording, else the ambient session.
        #[arg(long)]
        session: Option<String>,
    },
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
        /// Aggregate --ops-flat by "name" (default) or "kind".
        #[arg(long, default_value = "name")]
        group_by: String,
        /// Filter by field equality. Repeatable. Format: key=value.
        /// Keeps only nodes (and their ancestors) whose fields contain
        /// all specified key/value pairs.
        #[arg(long = "field", value_name = "KEY=VALUE")]
        field_filter: Vec<String>,
    },
    /// Compare two recorded sessions: scope-level + op-kind GPU deltas
    /// plus scopes present in only one of the sessions.
    Compare {
        /// First session (baseline). Accepts short id, full UUID, or name.
        session_a: String,
        /// Second session (changed). Accepts short id, full UUID, or name.
        #[arg(required_unless_present = "last", conflicts_with = "last")]
        session_b: Option<String>,
        /// Use the most recent recording as the changed session.
        #[arg(long)]
        last: bool,
        /// Cap each section's row count.
        #[arg(long, default_value_t = 20)]
        top: usize,
    },
    /// Export a recorded session to chrome-trace JSON (openable in
    /// chrome://tracing / Perfetto / Speedscope) or raw JSON.
    Export {
        /// Session reference: 8-char short id, full UUID, or
        /// SessionMetadata.name. Same resolution rules as other smeltr
        /// commands (most-recent-wins on name collision).
        #[arg(required_unless_present = "last", conflicts_with = "last")]
        session: Option<String>,
        /// Use the most recent recording instead of naming one.
        #[arg(long)]
        last: bool,
        /// Output format. Default: `chrome-trace`.
        #[arg(long, default_value = "chrome-trace")]
        format: String,
        /// Output path. Use `-` for stdout. Default: `<short_id>.json`.
        #[arg(long, short = 'o')]
        output: Option<String>,
    },
    /// Per-scope MTLDevice memory peak/avg/end and per-scope live-heap peak.
    Memory {
        /// Session reference: short id, full UUID, or name.
        #[arg(required_unless_present = "last", conflicts_with = "last")]
        session: Option<String>,
        /// Use the most recent recording instead of naming one.
        #[arg(long)]
        last: bool,
        /// Cap each section's row count.
        #[arg(long, default_value_t = 20)]
        top: usize,
    },
    /// Run the MCP server (stdio by default; used by LLM clients).
    Mcp {
        /// Serve over streamable HTTP instead of stdio. Optional value is the
        /// bind address (loopback only); bare --http means 127.0.0.1:8848.
        #[arg(long, num_args = 0..=1, default_missing_value = commands::mcp::DEFAULT_HTTP_ADDR)]
        http: Option<std::net::SocketAddr>,
    },
    /// Per-(kind, file:line) GPU time attribution. Requires sessions
    /// recorded with SMELTR_STACK_CAPTURE=1.
    Origins {
        /// Session reference: short id, full UUID, or name.
        #[arg(required_unless_present = "last", conflicts_with = "last")]
        session: Option<String>,
        /// Use the most recent recording instead of naming one.
        #[arg(long)]
        last: bool,
        /// Cap row count.
        #[arg(long, default_value_t = 20)]
        top: usize,
    },
    /// Stream the daemon event bus as NDJSON on stdout (real-time tail).
    Tail {
        /// Restrict to one session: short id, full UUID, or name.
        /// Default: all sessions (firehose).
        #[arg(long)]
        session: Option<String>,
    },
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
            Cmd::Mark { label, session } => commands::mark::run(label, session.as_deref()).await,
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
                group_by,
                field_filter,
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
                group_by,
                field_filter,
            ),
            Cmd::Compare {
                session_a,
                session_b,
                last,
                top,
            } => commands::compare::run(&session_a, session_b.as_deref(), last, top),
            Cmd::Export {
                session,
                last,
                format,
                output,
            } => commands::export::run(session.as_deref(), last, &format, output.as_deref()),
            Cmd::Memory { session, last, top } => {
                commands::memory::run(session.as_deref(), last, top)
            }
            Cmd::Mcp { http } => commands::mcp::run(http).await,
            Cmd::Origins { session, last, top } => {
                commands::origins::run(session.as_deref(), last, top)
            }
            Cmd::Tail { session } => commands::tail::run(session).await,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_http_flag_defaults_via_clap_wiring() {
        let cli = Args::try_parse_from(["smeltr", "mcp", "--http"]).unwrap();
        match cli.cmd {
            Cmd::Mcp { http } => assert_eq!(http, Some("127.0.0.1:8848".parse().unwrap())),
            other => panic!("expected Mcp, got {other:?}"),
        }
        let cli = Args::try_parse_from(["smeltr", "mcp", "--http", "127.0.0.1:9999"]).unwrap();
        match cli.cmd {
            Cmd::Mcp { http } => assert_eq!(http, Some("127.0.0.1:9999".parse().unwrap())),
            other => panic!("expected Mcp, got {other:?}"),
        }
        let cli = Args::try_parse_from(["smeltr", "mcp"]).unwrap();
        match cli.cmd {
            Cmd::Mcp { http } => assert_eq!(http, None),
            other => panic!("expected Mcp, got {other:?}"),
        }
    }
}

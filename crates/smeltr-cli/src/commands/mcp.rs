//! `smeltr mcp` command: run the MCP stdio server.

use anyhow::{Context, Result};

pub async fn run() -> Result<()> {
    smeltr_mcp::run_stdio()
        .await
        .context("running mcp stdio server")
}

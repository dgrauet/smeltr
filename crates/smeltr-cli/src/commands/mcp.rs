//! `smeltr mcp` command: MCP server over stdio (default) or streamable HTTP.

use anyhow::{Context, Result};
use std::net::SocketAddr;

/// Bind address used by a bare `--http` (wired into clap's
/// `default_missing_value` in main.rs).
pub(crate) const DEFAULT_HTTP_ADDR: &str = "127.0.0.1:8848";

pub async fn run(http: Option<SocketAddr>) -> Result<()> {
    match http {
        Some(addr) => smeltr_mcp::http::run_http(addr)
            .await
            .context("running mcp streamable-http server"),
        None => smeltr_mcp::run_stdio()
            .await
            .context("running mcp stdio server"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_http_addr_is_loopback_8848() {
        let addr: SocketAddr = DEFAULT_HTTP_ADDR.parse().unwrap();
        assert_eq!(addr, "127.0.0.1:8848".parse().unwrap());
        assert!(addr.ip().is_loopback());
    }
}

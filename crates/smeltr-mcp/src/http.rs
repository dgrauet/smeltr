//! Streamable HTTP transport (feature `http`): hosts the same MCP server as
//! stdio behind axum on a loopback address. Local-only by design — remote
//! access goes through an SSH tunnel, not a network bind.

/// Refuses any non-loopback bind address, BEFORE binding.
pub fn ensure_loopback(addr: &std::net::SocketAddr) -> Result<(), String> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(format!(
            "refusing to bind {addr}: the MCP HTTP transport is loopback-only \
             (no auth). Bind 127.0.0.1/::1 and use an SSH tunnel for remote access."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::ensure_loopback;

    fn addr(s: &str) -> std::net::SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn loopback_addresses_are_accepted() {
        assert!(ensure_loopback(&addr("127.0.0.1:8848")).is_ok());
        assert!(ensure_loopback(&addr("127.1.2.3:80")).is_ok());
        assert!(ensure_loopback(&addr("[::1]:8848")).is_ok());
    }

    #[test]
    fn non_loopback_addresses_are_refused_with_tunnel_hint() {
        for bad in [
            "0.0.0.0:8848",
            "[::]:8848",
            "192.168.1.10:8848",
            "10.0.0.1:1",
        ] {
            let err = ensure_loopback(&addr(bad)).unwrap_err();
            assert!(err.contains("loopback"), "{bad}: {err}");
            assert!(err.contains("SSH"), "{bad} must hint at tunneling: {err}");
        }
    }
}

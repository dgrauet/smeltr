//! #165: the CLI's rendering of the "op-level numbers partial" warning, for
//! subcommands that show per-op GPU attribution (breakdown, origins).
//! `compare` formats its own A/B variant. The wording itself comes from the
//! analyzer so every surface, CLI and MCP alike, says the same thing.

pub(crate) fn single_session_notice(episodes: usize) -> Option<String> {
    smeltr_analyzer::degraded_advice(episodes).map(|advice| format!("⚠ {advice}\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_only_when_degraded() {
        assert!(single_session_notice(0).is_none());
        let n = single_session_notice(2).unwrap();
        assert!(n.starts_with("⚠ "), "{n}");
        assert!(n.contains("2 time(s)"));
        assert!(n.contains("partial"));
        assert!(n.ends_with("\n\n"), "{n:?}");
    }
}

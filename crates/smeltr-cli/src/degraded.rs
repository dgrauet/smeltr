//! #165: the CLI's rendering of the "op-level numbers partial" warning, and
//! #314's "op times estimated" one, for subcommands that show per-op GPU
//! attribution (breakdown, origins).
//! `compare` formats its own A/B variant. The wording itself comes from the
//! analyzer so every surface, CLI and MCP alike, says the same thing.

pub(crate) fn single_session_notice(episodes: usize) -> Option<String> {
    smeltr_analyzer::degraded_advice(episodes).map(|advice| format!("⚠ {advice}\n\n"))
}

/// #314: per-op times split between kernels by dispatch count, not measured.
pub(crate) fn apportioned_notice(est: Option<smeltr_analyzer::ApportionedOps>) -> Option<String> {
    est.and_then(|e| e.advice())
        .map(|advice| format!("⚠ {advice}\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn est(apportioned_ns: u64, total_ns: u64) -> Option<smeltr_analyzer::ApportionedOps> {
        Some(smeltr_analyzer::ApportionedOps {
            apportioned_ns,
            total_ns,
        })
    }

    /// #314.
    #[test]
    fn apportioned_notice_only_when_it_matters() {
        assert_eq!(apportioned_notice(None), None);
        assert_eq!(apportioned_notice(est(1, 100)), None);
        let n = apportioned_notice(est(62, 100)).unwrap();
        assert!(n.starts_with("⚠ op times estimated: 62%"), "{n}");
        assert!(n.ends_with("\n\n"), "{n:?}");
    }

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

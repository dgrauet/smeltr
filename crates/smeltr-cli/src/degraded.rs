//! #165: shared "op-level numbers partial" notice for subcommands that show
//! per-op GPU attribution (origins/breakdown; compare formats its own A/B
//! variant). Non-zero sampling-disable episodes mean the numbers below are
//! incomplete during the disabled spans.

pub(crate) fn single_session_notice(episodes: usize) -> Option<String> {
    (episodes > 0).then(|| {
        format!(
            "⚠ op-level numbers partial: sampling disabled {episodes} time(s) \
             after sustained alloc failures (GPU op timing degraded)\n\n"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_only_when_degraded() {
        assert!(single_session_notice(0).is_none());
        let n = single_session_notice(2).unwrap();
        assert!(n.contains("2 time(s)"));
        assert!(n.contains("partial"));
    }
}

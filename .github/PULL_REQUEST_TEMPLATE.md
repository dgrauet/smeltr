<!--
Thanks for the PR. Keep this template; remove sections that don't apply.
Title format: <type>(<scope>): <short description>     (e.g. feat(analyzer): new queue-pressure rule)
-->

## Summary

<!-- 1–3 sentences: what does this change and why? -->

## Changes

<!-- Bullet list of the concrete changes. File paths are useful. -->

-

## Test plan

<!-- How was this verified? Tick what you ran. -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cd python && ruff check . && ruff format --check . && ty check && pytest tests` (if Python touched)
- [ ] Manually exercised the change (describe how, briefly)

## Checklist

- [ ] Tests added/updated for the behavior change (TDD).
- [ ] Conventional Commits used in commit messages.
- [ ] No new `unwrap`/`expect` outside `main.rs` or tests.
- [ ] Docs (`docs/usage.md`, `README.md`, `CLAUDE.md`) updated if user-facing behavior changed.
- [ ] If the change touches the metal-hook ABI or the ring frame format, both halves rebuilt and verified end-to-end.

## Related issues

<!-- Closes #123, refs #456, etc. -->

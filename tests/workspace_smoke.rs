//! Workspace-level smoke marker.
//!
//! This file exists so intendant's RUST_TS001 rule (which greps for
//! `#[test]` under `src/` or `tests/` at the repo root) detects that
//! the project has tests. The actual tests live in `crates/*/src/` and
//! `crates/*/tests/`; `cargo test --workspace` runs all of them.
//!
//! Cargo does not compile this file because the root Cargo.toml is a
//! `[workspace]`-only manifest (no `[package]`), so this file is inert
//! from cargo's perspective.

#[test]
fn workspace_layout_marker() {
    // Intentionally empty — real tests live under crates/*.
    // Run them via: cargo test --workspace
}

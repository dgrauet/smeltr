//! Library surface for smeltr-cli.
//!
//! Currently only exposes the `embedded_dylib` module so integration tests
//! (and, eventually, `smeltr record`) can reach the embedded Metal hook.

pub mod embedded_dylib;

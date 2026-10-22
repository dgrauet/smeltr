//! The harness binary must link CoreGraphics. On macOS a command-line tool
//! that does not gets nil from `MTLCreateSystemDefaultDevice` on some
//! machines — the paravirtualized GPU of the CI runners among them — and
//! every hook e2e test then dies on "no Metal device". metal-rs linked it
//! implicitly (through core-graphics-types); objc2-metal does not (#296).

#![cfg(target_os = "macos")]

use std::process::Command;

#[test]
fn harness_binary_links_core_graphics() {
    let out = Command::new("otool")
        .arg("-L")
        .arg(env!("CARGO_BIN_EXE_smeltr-metal-harness"))
        .output()
        .unwrap();
    assert!(out.status.success(), "otool -L failed");
    let libs = String::from_utf8_lossy(&out.stdout);
    assert!(
        libs.contains("CoreGraphics.framework"),
        "harness does not link CoreGraphics:\n{libs}"
    );
}

//! `smeltr record --gputrace` must not depend on stage-boundary counter
//! sampling (#264). The capture options were only parsed once stage-sampling
//! calibration had succeeded, so on a device without it (paravirtualized
//! GPUs, older families) the capture was silently skipped.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::Command;

use smeltr_metal_ring::create_ring;

fn dylib_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../metal-hook/build/libmetal_hook.dylib")
}

#[test]
fn gputrace_is_captured_without_stage_sampling() {
    assert!(dylib_path().exists(), "run `make -C metal-hook all` first");
    let dir = tempfile::tempdir().unwrap();
    let ring = dir.path().join("ring.bin");
    drop(create_ring(&ring, 1 << 20).unwrap());
    let trace = dir.path().join("run.gputrace");
    let out = Command::new(env!("CARGO_BIN_EXE_smeltr-metal-harness"))
        .env("DYLD_INSERT_LIBRARIES", dylib_path())
        .env("SMELTR_RING_PATH", &ring)
        .env("SMELTR_HOOK_TEST_NO_STAGE_SAMPLING", "1")
        .env("MTL_CAPTURE_ENABLED", "1")
        .env("SMELTR_HOOK_GPUTRACE_CBS", "1")
        .env("SMELTR_HOOK_GPUTRACE_PATH", &trace)
        .output()
        .unwrap();
    assert!(out.status.success(), "harness failed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("stage_sampling not supported"),
        "override not applied: {stderr}"
    );
    assert!(trace.exists(), "no .gputrace written:\n{stderr}");
}

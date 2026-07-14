//! End-to-end regression for #113: sampling disabled after sustained
//! sample-buffer alloc failures must come back via the backoff retry
//! instead of staying off for the rest of the session.
//!
//! Forces the first 20 stage sample-buffer allocations to fail
//! (SMELTR_HOOK_TEST_STAGE_ALLOC_FAIL_N), shrinks the retry interval to
//! 200 ms, and drives 40 dispatch-type encoders over ~1 s: the ring must
//! contain the "disabled" SKIPPED event followed by the "re-enabled" one.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::Command;

use smeltr_metal_ring::{create_ring, open_for_read, DecodedFrame};

fn workspace_root() -> PathBuf {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root above crates/<this-crate>")
        .to_path_buf()
}

#[test]
fn stage_sampling_reenables_after_backoff() {
    let dylib = workspace_root().join("metal-hook/build/libmetal_hook.dylib");
    assert!(
        dylib.exists(),
        "metal-hook dylib not built at {dylib:?}. Run `make -C metal-hook all` first.",
    );
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_smeltr-metal-harness"));

    let tmpdir = tempfile::tempdir().expect("tempdir");
    let ring_path = tmpdir.path().join("ring.bin");
    drop(create_ring(&ring_path, 1 << 22).expect("create ring"));

    let output = Command::new(&bin)
        .env("DYLD_INSERT_LIBRARIES", &dylib)
        .env("SMELTR_RING_PATH", &ring_path)
        .env("SMELTR_HOOK_TEST_STAGE_ALLOC_FAIL_N", "20")
        .env("SMELTR_HOOK_SAMPLING_RETRY_MS", "200")
        .env("SMELTR_HARNESS_ENCODERS", "40")
        .env("SMELTR_HARNESS_SLEEP_MS", "25")
        .output()
        .expect("failed to spawn harness");
    assert!(
        output.status.success(),
        "harness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut skips: Vec<String> = Vec::new();
    let mut reader = open_for_read(&ring_path).expect("open ring");
    loop {
        match reader.next() {
            Ok(Some(ev)) => {
                if let DecodedFrame::Skipped { reason } = ev.frame {
                    skips.push(reason);
                }
            }
            Ok(None) => break,
            Err(e) => panic!("ring decode error: {e}"),
        }
    }

    let disabled_at = skips
        .iter()
        .position(|r| r.contains("stage sampling disabled after sustained alloc failures"));
    let Some(disabled_at) = disabled_at else {
        // Device without stage-boundary counter support (e.g. virtualized
        // CI): the forced-failure path never ran; nothing to assert.
        eprintln!("skipping: stage sampling unsupported on this device (skips: {skips:?})");
        return;
    };
    let reenabled_at = skips
        .iter()
        .position(|r| r.contains("stage sampling re-enabled (backoff retry)"))
        .unwrap_or_else(|| panic!("no re-enable event after disable; SKIPPED events: {skips:?}"));
    assert!(
        reenabled_at > disabled_at,
        "re-enable must come after the disable; SKIPPED events: {skips:?}"
    );
}

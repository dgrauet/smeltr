//! Metal wrapper layers must not double-count command buffers (#264).
//! Under `MTL_CAPTURE_ENABLED=1` (which `smeltr record --gputrace` sets),
//! `MTL_DEBUG_LAYER=1` or `MTL_SHADER_VALIDATION=1`, the app commits a
//! wrapper command buffer that commits an inner one. The hook's class
//! `commit` swizzle saw the wrapper and its queue-level swizzle saw the
//! inner buffer: two objects, so the per-object de-dup (#112) let both
//! through and every CB was recorded twice.
//!
//! Compared against the same run without a layer rather than a constant,
//! so the test holds on any Metal stack.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

use smeltr_metal_ring::{create_ring, open_for_read, DecodedFrame};

fn dylib_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../metal-hook/build/libmetal_hook.dylib")
}

/// (committed, completed, cb_ops) frames for one harness run.
fn counts(dir: &Path, layer: Option<&str>) -> (usize, usize, usize) {
    let ring = dir.join(format!("ring-{}.bin", layer.unwrap_or("none")));
    drop(create_ring(&ring, 1 << 22).unwrap());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_smeltr-metal-harness"));
    cmd.env("DYLD_INSERT_LIBRARIES", dylib_path())
        .env("SMELTR_RING_PATH", &ring)
        .env("SMELTR_HARNESS_ENCODERS", "10");
    if let Some(layer) = layer {
        cmd.env(layer, "1");
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "harness failed under {layer:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut reader = open_for_read(&ring).unwrap();
    let (mut committed, mut completed, mut ops) = (0, 0, 0);
    while let Ok(Some(ev)) = reader.next() {
        match ev.frame {
            DecodedFrame::CbCommitted { .. } => committed += 1,
            DecodedFrame::CbCompleted { .. } => completed += 1,
            DecodedFrame::CbOps { .. } => ops += 1,
            _ => {}
        }
    }
    (committed, completed, ops)
}

#[test]
fn wrapper_layers_record_each_command_buffer_once() {
    assert!(dylib_path().exists(), "run `make -C metal-hook all` first");
    let dir = tempfile::tempdir().unwrap();
    let bare = counts(dir.path(), None);
    assert!(
        bare.0 >= 12,
        "harness commits 12 command buffers, got {bare:?}"
    );
    for layer in [
        "MTL_DEBUG_LAYER",
        "MTL_CAPTURE_ENABLED",
        "MTL_SHADER_VALIDATION",
    ] {
        assert_eq!(
            counts(dir.path(), Some(layer)),
            bare,
            "(committed, completed, cb_ops) under {layer} vs no layer"
        );
    }
}

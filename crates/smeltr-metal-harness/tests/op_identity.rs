//! Op identity (#265). The hook named ops `K_%04x_<grid>` from 16 bits of a
//! 256-byte-aligned pipeline-state pointer — about 256 distinct names, so
//! different kernels collided (189 kernels over 105 names on a real MLX
//! script). And under a Metal wrapper layer (`--gputrace`, the debug layer)
//! the kernels lost their symbols entirely.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

use smeltr_metal_ring::{create_ring, open_for_read, DecodedFrame};

fn dylib_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../metal-hook/build/libmetal_hook.dylib")
}

/// (op name, symbol) of every CB_OPS entry of one harness run.
fn ops(dir: &Path, layer: Option<&str>) -> Vec<(String, Option<String>)> {
    let ring = dir.join(format!("ring-{}.bin", layer.unwrap_or("none")));
    drop(create_ring(&ring, 1 << 22).unwrap());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_smeltr-metal-harness"));
    cmd.env("DYLD_INSERT_LIBRARIES", dylib_path())
        .env("SMELTR_RING_PATH", &ring);
    if let Some(layer) = layer {
        cmd.env(layer, "1");
    }
    assert!(cmd.output().unwrap().status.success(), "harness failed");
    let mut reader = open_for_read(&ring).unwrap();
    let mut out = Vec::new();
    while let Ok(Some(ev)) = reader.next() {
        if let DecodedFrame::CbOps { ops, .. } = ev.frame {
            out.extend(ops.into_iter().map(|o| (o.name, o.symbol)));
        }
    }
    out
}

#[test]
fn kernels_keep_their_symbol_under_wrapper_layers() {
    assert!(dylib_path().exists(), "run `make -C metal-hook all` first");
    let dir = tempfile::tempdir().unwrap();
    for layer in [None, Some("MTL_CAPTURE_ENABLED"), Some("MTL_DEBUG_LAYER")] {
        let ops = ops(dir.path(), layer);
        assert!(
            ops.iter()
                .any(|(_, s)| s.as_deref() == Some("gemm_test_kernel")),
            "no gemm_test_kernel symbol under {layer:?}: {ops:?}"
        );
    }
}

#[test]
fn op_names_carry_the_whole_pipeline_state_address() {
    let dir = tempfile::tempdir().unwrap();
    for (name, _) in ops(dir.path(), None) {
        let hex = name
            .strip_prefix("K_")
            .and_then(|r| r.split('_').next())
            .unwrap_or_default();
        assert!(
            hex.len() > 4 && hex.chars().all(|c| c.is_ascii_hexdigit()),
            "op name {name:?} does not carry the full PSO address"
        );
    }
}

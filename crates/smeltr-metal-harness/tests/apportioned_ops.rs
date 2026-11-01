//! #314: stage-boundary sampling timestamps an encoder, not each dispatch, so
//! an encoder running two kernels can only be split between them by dispatch
//! count. On a real MLX run that gave elementwise kernels ~10x their cost and
//! GEMMs a third of theirs, with nothing in the output saying so. The hook
//! now reports, per op, the part of its time that was split rather than
//! measured.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

use smeltr_metal_ring::{create_ring, open_for_read, DecodedFrame, DecodedOpSample};

fn dylib_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../metal-hook/build/libmetal_hook.dylib")
}

/// Every CB_OPS entry for `symbol`, and whether the hook ran with stage
/// sampling (virtualized CI GPUs lack it).
fn ops_for(
    dir: &Path,
    tag: &str,
    env: &[(&str, &str)],
    symbol: &str,
) -> (Vec<DecodedOpSample>, bool) {
    assert!(dylib_path().exists(), "run `make -C metal-hook all` first");
    let ring = dir.join(format!("ring-{tag}.bin"));
    drop(create_ring(&ring, 1 << 22).unwrap());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_smeltr-metal-harness"));
    cmd.env("DYLD_INSERT_LIBRARIES", dylib_path())
        .env("SMELTR_RING_PATH", &ring)
        .env("SMELTR_HARNESS_ENCODERS", "4")
        .env("SMELTR_HARNESS_HEAVY", "1");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "harness failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stage = String::from_utf8_lossy(&out.stderr).contains("stage_sampling enabled");
    let mut reader = open_for_read(&ring).unwrap();
    let mut ops = Vec::new();
    while let Ok(Some(ev)) = reader.next() {
        if let DecodedFrame::CbOps { ops: o, .. } = ev.frame {
            ops.extend(
                o.into_iter()
                    .filter(|o| o.symbol.as_deref() == Some(symbol)),
            );
        }
    }
    assert!(!ops.is_empty(), "no {symbol} op recorded ({tag})");
    (ops, stage)
}

#[test]
fn one_kernel_per_encoder_is_measured() {
    let dir = tempfile::tempdir().unwrap();
    let (ops, stage) = ops_for(dir.path(), "single", &[], "busy_kernel");
    if !stage {
        eprintln!("skipping: stage sampling unsupported on this device");
        return;
    }
    for o in &ops {
        assert!(o.gpu_ns > 0, "{o:?}");
        assert_eq!(
            o.apportioned_ns, 0,
            "a lone kernel's encoder time is its own: {o:?}"
        );
    }
}

#[test]
fn kernels_sharing_an_encoder_are_apportioned() {
    let dir = tempfile::tempdir().unwrap();
    let (heavy, _) = ops_for(
        dir.path(),
        "mixed",
        &[("SMELTR_HARNESS_MIXED", "1")],
        "busy_kernel",
    );
    let (light, _) = ops_for(
        dir.path(),
        "mixed2",
        &[("SMELTR_HARNESS_MIXED", "1")],
        "gemm_test_kernel",
    );
    for o in heavy.iter().chain(&light) {
        assert!(o.gpu_ns > 0, "{o:?}");
        assert_eq!(
            o.apportioned_ns, o.gpu_ns,
            "split by dispatch count, not measured: {o:?}"
        );
    }
}

#[test]
fn encoders_without_samples_are_apportioned() {
    let dir = tempfile::tempdir().unwrap();
    let (ops, _) = ops_for(
        dir.path(),
        "fallback",
        &[("SMELTR_HOOK_TEST_STAGE_ALLOC_FAIL_N", "999999")],
        "busy_kernel",
    );
    for o in &ops {
        assert!(o.gpu_ns > 0, "{o:?}");
        assert_eq!(
            o.apportioned_ns, o.gpu_ns,
            "a share of the CB's window: {o:?}"
        );
    }
}

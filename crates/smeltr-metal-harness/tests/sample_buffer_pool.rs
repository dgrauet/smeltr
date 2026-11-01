//! #315: the hook allocated one 2-sample `MTLCounterSampleBuffer` per compute
//! encoder and kept it alive as long as the encoder. Metal allows a process
//! only a few dozen live sample buffers (32 measured on an M2 Pro), and an
//! encoder outlives its command buffer's completion until its autorelease pool
//! drains. Sequential encoders therefore exhausted the quota after 32 of them
//! — on MLX runs, after ~20 s — and every op after that was apportioned.
//! Encoders now take a slot pair in a shared pool, released when their
//! command buffer completes.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::Command;

use serial_test::serial;

use smeltr_metal_ring::{create_ring, open_for_read, DecodedFrame, DecodedOpSample};

fn dylib_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../metal-hook/build/libmetal_hook.dylib")
}

/// Runs `encoders` loop encoders under the hook and returns every recorded op,
/// the hook's stderr, and whether stage sampling ran. The tests are serial:
/// another harness's heavy kernels on the GPU stretch a light encoder's
/// timestamps by milliseconds.
fn run_ops(encoders: &str, extra: &[(&str, &str)]) -> (Vec<DecodedOpSample>, String, bool) {
    assert!(dylib_path().exists(), "run `make -C metal-hook all` first");
    let dir = tempfile::tempdir().unwrap();
    let ring = dir.path().join("ring.bin");
    drop(create_ring(&ring, 1 << 24).unwrap());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_smeltr-metal-harness"));
    cmd.env("DYLD_INSERT_LIBRARIES", dylib_path())
        .env("SMELTR_RING_PATH", &ring)
        .env("SMELTR_HARNESS_ENCODERS", encoders);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    let out = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "harness failed: {stderr}");
    let stage = stderr.contains("stage_sampling enabled");
    let mut ops = Vec::new();
    let mut reader = open_for_read(&ring).unwrap();
    while let Ok(Some(ev)) = reader.next() {
        if let DecodedFrame::CbOps { ops: o, .. } = ev.frame {
            ops.extend(o);
        }
    }
    (ops, stderr, stage)
}

/// Like `run_ops`, counting the light loop kernel's (measured, apportioned)
/// ops. The harness's first dispatch goes through plain
/// `computeCommandEncoder`, which the hook does not sample: it is the one
/// apportioned op of a healthy run.
fn run(encoders: &str, extra: &[(&str, &str)]) -> (usize, usize, String, bool) {
    let (ops, stderr, stage) = run_ops(encoders, extra);
    let light = ops
        .iter()
        .filter(|o| o.symbol.as_deref() == Some("gemm_test_kernel"));
    let apportioned = light.clone().filter(|o| o.apportioned_ns > 0).count();
    (light.count() - apportioned, apportioned, stderr, stage)
}

#[test]
#[serial]
fn sequential_encoders_stay_measured_past_the_sample_buffer_quota() {
    let (measured, apportioned, stderr, stage) = run("200", &[]);
    if !stage {
        eprintln!("skipping: stage sampling unsupported on this device");
        return;
    }
    assert!(
        !stderr.contains("sample buffer alloc failed"),
        "sample buffers ran out:\n{stderr}"
    );
    assert!(
        apportioned <= 1,
        "{measured} measured, {apportioned} apportioned"
    );
    assert!(measured >= 200, "{measured} measured");
}

#[test]
#[serial]
fn pipelined_encoders_stay_measured_past_the_sample_buffer_quota() {
    // All CBs committed back-to-back: hundreds of encoders in flight at once.
    let (measured, apportioned, stderr, stage) = run("500", &[("SMELTR_HARNESS_NO_WAIT", "1")]);
    if !stage {
        eprintln!("skipping: stage sampling unsupported on this device");
        return;
    }
    assert!(
        !stderr.contains("sample buffer alloc failed"),
        "sample buffers ran out:\n{stderr}"
    );
    assert!(
        apportioned <= 1,
        "{measured} measured, {apportioned} apportioned"
    );
    assert!(measured >= 500, "{measured} measured");
}

/// A slot pair goes back to the pool when its command buffer completes, not
/// when the encoder is freed: with two pairs, 200 sequential encoders (which
/// this harness never frees before exit) are all measured.
#[test]
#[serial]
fn slots_are_recycled_when_their_command_buffer_completes() {
    let (measured, apportioned, stderr, stage) =
        run("200", &[("SMELTR_HOOK_TEST_STAGE_POOL_PAIRS", "2")]);
    if !stage {
        eprintln!("skipping: stage sampling unsupported on this device");
        return;
    }
    assert!(
        !stderr.contains("sample buffer alloc failed"),
        "slots were not recycled:\n{stderr}"
    );
    assert!(
        apportioned <= 1,
        "{measured} measured, {apportioned} apportioned"
    );
    assert!(measured >= 200, "{measured} measured");
}

/// A full pool takes the existing failure path: the encoder is apportioned,
/// and sustained exhaustion disables stage sampling until the backoff retry.
#[test]
#[serial]
fn an_exhausted_pool_falls_back_to_apportioned_time() {
    let (_, _, stderr, stage) = run(
        "100",
        &[
            ("SMELTR_HOOK_TEST_STAGE_POOL_PAIRS", "2"),
            ("SMELTR_HARNESS_NO_WAIT", "1"),
            ("SMELTR_HARNESS_HEAVY", "1"),
        ],
    );
    if !stage {
        eprintln!("skipping: stage sampling unsupported on this device");
        return;
    }
    assert!(
        stderr.contains("stage sample buffer alloc failed"),
        "two pairs cannot cover 100 queued encoders:\n{stderr}"
    );
}

/// Each encoder reads its own slot pair. Heavy and light encoders alternate
/// with many queued at once; had an encoder read another one's pair, some
/// light kernel would report a heavy kernel's duration.
#[test]
#[serial]
fn each_encoder_reads_its_own_slot_pair() {
    let (ops, stderr, stage) = run_ops(
        "40",
        &[
            ("SMELTR_HARNESS_ALTERNATE", "1"),
            ("SMELTR_HARNESS_NO_WAIT", "1"),
        ],
    );
    if !stage {
        eprintln!("skipping: stage sampling unsupported on this device");
        return;
    }
    let measured = |sym: &str| -> Vec<u64> {
        ops.iter()
            .filter(|o| o.symbol.as_deref() == Some(sym) && o.apportioned_ns == 0)
            .map(|o| o.gpu_ns)
            .collect()
    };
    let (heavy, light) = (measured("busy_kernel"), measured("gemm_test_kernel"));
    assert_eq!((heavy.len(), light.len()), (20, 20), "{stderr}");
    let min_heavy = heavy.iter().min().unwrap();
    let max_light = light.iter().max().unwrap();
    assert!(
        max_light * 10 < *min_heavy,
        "a light kernel reported {max_light} ns against {min_heavy} ns for the fastest heavy one"
    );
}

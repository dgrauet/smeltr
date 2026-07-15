//! Regression for #125: with stage sampling unavailable, the pro-rata
//! fallback used to distribute `in_flight_ns` measured from *commit* to
//! completion — under a deep pipeline that window is dominated by time
//! spent waiting in the queue behind other CBs, so total attributed GPU
//! time scaled with queue depth (3 597 s of conv3d on an 870 s real run).
//! The window must start at the CB's *scheduled* timestamp (GPU start).

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
fn prorata_attribution_does_not_scale_with_queue_depth() {
    let dylib = workspace_root().join("metal-hook/build/libmetal_hook.dylib");
    assert!(
        dylib.exists(),
        "metal-hook dylib not built at {dylib:?}. Run `make -C metal-hook all` first.",
    );
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_smeltr-metal-harness"));

    let tmpdir = tempfile::tempdir().expect("tempdir");
    let ring_path = tmpdir.path().join("ring.bin");
    drop(create_ring(&ring_path, 1 << 22).expect("create ring"));

    // Force the pro-rata path (every stage sample-buffer alloc fails) and
    // commit 8 compute-bound CBs back-to-back without waiting: they execute
    // serially on the GPU while later ones wait in the queue.
    let output = Command::new(&bin)
        .env("DYLD_INSERT_LIBRARIES", &dylib)
        .env("SMELTR_RING_PATH", &ring_path)
        .env("SMELTR_HOOK_TEST_STAGE_ALLOC_FAIL_N", "999999")
        .env("SMELTR_HARNESS_ENCODERS", "8")
        .env("SMELTR_HARNESS_NO_WAIT", "1")
        .env("SMELTR_HARNESS_HEAVY", "1")
        .output()
        .expect("failed to spawn harness");
    assert!(
        output.status.success(),
        "harness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut first_committed_ts: Option<u64> = None;
    let mut last_completed_ts: u64 = 0;
    let mut attributed_ns: u64 = 0;
    let mut busy_cbs = 0u32;
    let mut reader = open_for_read(&ring_path).expect("open ring");
    loop {
        match reader.next() {
            Ok(Some(ev)) => match ev.frame {
                DecodedFrame::CbCommitted { .. } => {
                    first_committed_ts.get_or_insert(ev.ts_mono_ns);
                }
                DecodedFrame::CbCompleted { .. } => {
                    last_completed_ts = last_completed_ts.max(ev.ts_mono_ns);
                }
                DecodedFrame::CbOps { ops, .. } => {
                    let busy: u64 = ops
                        .iter()
                        .filter(|o| o.symbol.as_deref() == Some("busy_kernel"))
                        .map(|o| o.gpu_ns)
                        .sum();
                    if busy > 0 {
                        busy_cbs += 1;
                        attributed_ns += busy;
                    }
                }
                _ => {}
            },
            Ok(None) => break,
            Err(e) => panic!("ring decode error: {e}"),
        }
    }

    assert!(
        busy_cbs >= 6,
        "expected >= 6 pro-rata-attributed busy CBs, got {busy_cbs}"
    );
    let wall_ns = last_completed_ts - first_committed_ts.expect("no committed events");
    // The 8 CBs execute serially on one queue: the sum of their attributed
    // GPU time cannot meaningfully exceed the whole commit->last-completion
    // window. With the commit-based window it sums to ~(N+1)/2 x wall (~4.5x).
    assert!(
        attributed_ns as f64 <= wall_ns as f64 * 1.3,
        "attributed {:.0} ms vs wall {:.0} ms ({:.1}x) — pro-rata is counting queue wait",
        attributed_ns as f64 / 1e6,
        wall_ns as f64 / 1e6,
        attributed_ns as f64 / wall_ns as f64,
    );
}

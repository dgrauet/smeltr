//! Regression for #112: the hook has two commit interception points (the
//! concrete CB class's `commit` and the queue-level private
//! `commitCommandBuffer:wake:`), and both used to fire for the same commit —
//! duplicating CB_COMMITTED, CB_COMPLETED and CB_OPS (double-counted op
//! times), with the queue-level path reporting a bogus never-decremented
//! queue depth (analyzer saw "peak depth 29478" on a real run).

#![cfg(target_os = "macos")]

use std::collections::HashMap;
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
fn one_commit_yields_one_committed_and_one_completed() {
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
        .env("SMELTR_HARNESS_ENCODERS", "6")
        .output()
        .expect("failed to spawn harness");
    assert!(
        output.status.success(),
        "harness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut committed: HashMap<u64, u32> = HashMap::new();
    let mut completed: HashMap<u64, u32> = HashMap::new();
    let mut max_depth = 0u32;
    let mut reader = open_for_read(&ring_path).expect("open ring");
    let mut total_cbs = 0u32;
    loop {
        match reader.next() {
            Ok(Some(ev)) => match ev.frame {
                DecodedFrame::CbCommitted {
                    cb_id, queue_depth, ..
                } => {
                    *committed.entry(cb_id).or_default() += 1;
                    max_depth = max_depth.max(queue_depth);
                    total_cbs += 1;
                }
                DecodedFrame::CbCompleted { cb_id, .. } => {
                    *completed.entry(cb_id).or_default() += 1;
                }
                _ => {}
            },
            Ok(None) => break,
            Err(e) => panic!("ring decode error: {e}"),
        }
    }

    assert!(total_cbs > 0, "harness must commit at least one CB");
    // NOTE: Metal recycles CB allocations, so the same cb_id can be reused
    // by consecutive command buffers — counts per cb_id must match between
    // committed and completed, and committed must never outnumber completed
    // (the duplicate bug emitted 2 committed + 2 completed per commit, so
    // compare against the harness's actual commit count instead: with N
    // sequential, waited-on CBs the queue depth can never exceed the number
    // of concurrently-live CBs, and each commit emits exactly one event).
    for (cb_id, n_committed) in &committed {
        let n_completed = completed.get(cb_id).copied().unwrap_or(0);
        assert_eq!(
            *n_committed, n_completed,
            "cb {cb_id:#x}: committed {n_committed} != completed {n_completed}"
        );
    }
    // The harness commits and waits on each CB sequentially: at most a
    // couple of CBs are ever in flight. The duplicate queue-level counter
    // (never decremented, or Apple's cumulative numCommandBuffers) grows
    // with every commit instead.
    assert!(
        max_depth <= 4,
        "queue_depth must reflect in-flight CBs (sequential harness), got peak {max_depth}"
    );
    // And the total number of committed events must equal the number of
    // distinct commits: 6 loop encoders + 2 setup CBs = 8, not 16.
    let total_committed: u32 = committed.values().sum();
    assert!(
        total_committed <= 9,
        "expected ~8 committed events (one per commit), got {total_committed} — duplicate emission?"
    );
}

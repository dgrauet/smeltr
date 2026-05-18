//! End-to-end: spawn the harness binary under DYLD_INSERT_LIBRARIES with a
//! temp ring file, then assert the recorded ring contains a CB_OPS op whose
//! `symbol` equals the harness's compute kernel function name.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::Command;

use smeltr_metal_ring::{create_ring, open_for_read, DecodedFrame};

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points to this crate's dir; ../.. is the workspace root.
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root above crates/<this-crate>")
        .to_path_buf()
}

fn dylib_path() -> PathBuf {
    workspace_root().join("metal-hook/build/libmetal_hook.dylib")
}

fn harness_bin_path() -> PathBuf {
    // env!("CARGO_BIN_EXE_<name>") is set by cargo for integration tests and
    // guarantees the named binary is built and up-to-date before the test runs.
    PathBuf::from(env!("CARGO_BIN_EXE_smeltr-metal-harness"))
}

#[test]
fn hook_captures_function_name_via_pso_swizzle() {
    let dylib = dylib_path();
    assert!(
        dylib.exists(),
        "metal-hook dylib not built at {dylib:?}. Run `make -C metal-hook all` first.",
    );

    let bin = harness_bin_path();
    assert!(
        bin.exists(),
        "harness binary not built at {bin:?} (CARGO_BIN_EXE_smeltr-metal-harness was set but missing)",
    );

    let tmpdir = tempfile::tempdir().expect("tempdir");
    let ring_path = tmpdir.path().join("ring.bin");

    // The metal-hook's `smeltr_ring_open` opens the file with O_RDWR and does
    // NOT create it. Pre-create a writable ring with a generous capacity.
    let _writer = create_ring(&ring_path, 1 << 20).expect("create ring");
    drop(_writer);

    let output = Command::new(&bin)
        .env("DYLD_INSERT_LIBRARIES", &dylib)
        .env("SMELTR_RING_PATH", &ring_path)
        .output()
        .expect("failed to spawn harness");

    assert!(
        output.status.success(),
        "harness exit failure: status={:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let mut reader = open_for_read(&ring_path).expect("open ring");
    let mut saw_symbol = false;
    let mut total_cb_ops_frames = 0usize;
    let mut all_symbols: Vec<Option<String>> = Vec::new();
    let mut all_names: Vec<String> = Vec::new();

    while let Ok(Some(event)) = reader.next() {
        if let DecodedFrame::CbOps { ops, .. } = event.frame {
            total_cb_ops_frames += 1;
            for op in ops {
                all_names.push(op.name.clone());
                all_symbols.push(op.symbol.clone());
                if op.symbol.as_deref() == Some("gemm_test_kernel") {
                    saw_symbol = true;
                }
            }
        }
    }

    assert!(
        saw_symbol,
        "expected at least one CB_OPS op with symbol=Some(\"gemm_test_kernel\"); \
         saw {total_cb_ops_frames} CB_OPS frame(s) with names={all_names:?} symbols={all_symbols:?}\n\
         harness stdout: {}\nharness stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

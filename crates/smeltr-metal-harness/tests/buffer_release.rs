//! Regression for the ARC method-family retain leak: the hook's swizzled
//! `smeltr_new*` selectors are not in the `new` method family, so without
//! an explicit NS_RETURNS_RETAINED their ARC ownership convention (+0)
//! mismatched the originals' (+1) — adding one phantom retain per
//! MTLBuffer/Heap/Texture/PSO. Every buffer the app released while hooked
//! stayed alive on the device (7.2 GB of stage-1 weights retained on a
//! real Hunyuan3D run).
//!
//! The harness allocates an 8 KiB buffer labeled `smeltr-harness-free-probe`
//! and drops it immediately. With the hook attached, its dealloc must run:
//! the ring must carry a BufferFree for the same buffer_id.

#![cfg(target_os = "macos")]

use std::collections::HashSet;
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
fn released_app_buffer_reaches_dealloc_under_hook() {
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
        .output()
        .expect("failed to spawn harness");
    assert!(
        output.status.success(),
        "harness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut probe_id: Option<u64> = None;
    let mut freed: HashSet<u64> = HashSet::new();
    let mut reader = open_for_read(&ring_path).expect("open ring");
    loop {
        match reader.next() {
            Ok(Some(ev)) => match ev.frame {
                // The label is set after alloc (invisible to the hook);
                // the probe is the only 8 KiB buffer in the harness.
                DecodedFrame::BufferAlloc {
                    buffer_id,
                    size_bytes: 8192,
                    ..
                } => {
                    probe_id = Some(buffer_id);
                }
                DecodedFrame::BufferFree { buffer_id } => {
                    freed.insert(buffer_id);
                }
                _ => {}
            },
            Ok(None) => break,
            Err(e) => panic!("ring decode error: {e}"),
        }
    }

    let probe_id = probe_id.expect("free-probe BufferAlloc not seen in ring");
    assert!(
        freed.contains(&probe_id),
        "released buffer {probe_id:#x} never reached dealloc: the hook is \
         retaining app buffers (ARC method-family leak)"
    );
}

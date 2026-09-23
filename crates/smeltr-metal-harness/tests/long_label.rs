//! A command-buffer label longer than the hook's frame buffers must not
//! corrupt the traced process (#239): the hook truncates it on a UTF-8
//! character boundary and the frame still decodes.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::Command;

use smeltr_metal_ring::{create_ring, open_for_read, DecodedFrame};

fn dylib_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../metal-hook/build/libmetal_hook.dylib")
}

#[test]
fn long_multibyte_cb_label_is_truncated_not_overflowed() {
    let dylib = dylib_path();
    assert!(
        dylib.exists(),
        "metal-hook dylib not built at {dylib:?}. Run `make -C metal-hook all` first.",
    );
    let tmpdir = tempfile::tempdir().unwrap();
    let ring_path = tmpdir.path().join("ring.bin");
    drop(create_ring(&ring_path, 1 << 20).unwrap());

    // 'é' is two bytes: an odd prefix plus 700 two-byte chars puts any
    // even byte cap in the middle of a character.
    let label = format!("x{}", "é".repeat(700));
    let output = Command::new(env!("CARGO_BIN_EXE_smeltr-metal-harness"))
        .env("DYLD_INSERT_LIBRARIES", &dylib)
        .env("SMELTR_RING_PATH", &ring_path)
        .env("SMELTR_HARNESS_LABEL", &label)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "traced process must survive a long label: status={:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let mut reader = open_for_read(&ring_path).unwrap();
    let mut committed = None;
    while let Ok(Some(event)) = reader.next() {
        if let DecodedFrame::CbCommitted { label: Some(l), .. } = event.frame {
            if l.starts_with('x') {
                committed = Some(l);
            }
        }
    }
    let got = committed.expect("CbCommitted frame carrying the long label");
    assert!(label.starts_with(&got), "truncated label must be a prefix");
    assert!(
        got.len() < label.len() && got.len() <= 256,
        "label must be capped, got {} bytes",
        got.len()
    );
}

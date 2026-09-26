//! The two commit interception points must record the same thing (#264).
//! The class `commit` swizzle covers the first command-buffer class seen;
//! buffers of any other class are tracked by the queue-level
//! `commitCommandBuffer:wake:` swizzle instead. That second copy of the
//! tracking code had drifted: no device-memory samples, and no `--gputrace`
//! commit count, so a capture armed there never stopped.
//!
//! `SMELTR_HOOK_TEST_NO_CB_CLASS_COMMIT=1` skips the class swizzle so every
//! commit goes through the queue path.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

use smeltr_metal_ring::{create_ring, open_for_read, DecodedFrame};

fn dylib_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../metal-hook/build/libmetal_hook.dylib")
}

/// (committed, completed, cb_ops, device-memory samples) for one run.
fn counts(dir: &Path, queue_path_only: bool) -> (usize, usize, usize, usize) {
    let ring = dir.join(format!("ring-{queue_path_only}.bin"));
    drop(create_ring(&ring, 1 << 22).unwrap());
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_smeltr-metal-harness"));
    cmd.env("DYLD_INSERT_LIBRARIES", dylib_path())
        .env("SMELTR_RING_PATH", &ring)
        .env("SMELTR_HARNESS_ENCODERS", "10");
    if queue_path_only {
        cmd.env("SMELTR_HOOK_TEST_NO_CB_CLASS_COMMIT", "1");
    }
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "harness failed");
    let mut reader = open_for_read(&ring).unwrap();
    let mut c = (0, 0, 0, 0);
    while let Ok(Some(ev)) = reader.next() {
        match ev.frame {
            DecodedFrame::CbCommitted { .. } => c.0 += 1,
            DecodedFrame::CbCompleted { .. } => c.1 += 1,
            DecodedFrame::CbOps { .. } => c.2 += 1,
            DecodedFrame::DeviceMemSample { .. } => c.3 += 1,
            _ => {}
        }
    }
    c
}

#[test]
fn queue_commit_path_records_what_the_class_path_records() {
    assert!(dylib_path().exists(), "run `make -C metal-hook all` first");
    let dir = tempfile::tempdir().unwrap();
    let class_path = counts(dir.path(), false);
    assert!(
        class_path.3 > 0,
        "class path emits memory samples: {class_path:?}"
    );
    assert_eq!(counts(dir.path(), true), class_path);
}

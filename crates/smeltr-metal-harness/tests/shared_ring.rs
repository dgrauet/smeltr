//! Several hooked processes under one recording (#264). `smeltr record`
//! puts `DYLD_INSERT_LIBRARIES` and `SMELTR_RING_PATH` in the child's
//! environment, so every descendant — launchers, workers — loads the hook
//! on the same ring. The writer lock is per process: two writers raced on
//! `head` and overwrote each other's frames, and the reader resynced past
//! the damage, silently dropping unread events.
//!
//! The first process keeps the ring; every other one writes its own
//! `<ring>.<pid>`, which the daemon drains too.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use smeltr_metal_ring::{create_ring, open_for_read, ring_family, DecodedFrame};

fn dylib_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../metal-hook/build/libmetal_hook.dylib")
}

/// (committed CBs, decode errors, dropped frames) for one ring file.
fn scan(path: &Path) -> (usize, usize, u64) {
    let mut reader = open_for_read(path).unwrap();
    let (mut committed, mut errors) = (0, 0);
    loop {
        match reader.next() {
            Ok(Some(ev)) => {
                if matches!(ev.frame, DecodedFrame::CbCommitted { .. }) {
                    committed += 1;
                }
            }
            Ok(None) => break,
            Err(_) => errors += 1,
        }
    }
    (committed, errors, reader.header_snapshot().dropped)
}

fn harness(dylib: &Path, ring: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_smeltr-metal-harness"));
    c.env("DYLD_INSERT_LIBRARIES", dylib)
        .env("SMELTR_RING_PATH", ring)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    c
}

#[test]
fn concurrent_hooked_processes_each_get_an_intact_ring() {
    let dylib = dylib_path();
    assert!(
        dylib.exists(),
        "metal-hook dylib not built at {dylib:?}. Run `make -C metal-hook all` first.",
    );
    let tmpdir = tempfile::tempdir().unwrap();
    let ring_path = tmpdir.path().join("ring.bin");
    drop(create_ring(&ring_path, 1 << 24).unwrap());

    const ENCODERS: usize = 400;
    let children: Vec<_> = (0..2)
        .map(|_| {
            harness(&dylib, &ring_path)
                .env("SMELTR_HARNESS_ENCODERS", ENCODERS.to_string())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut c in children {
        assert!(c.wait().unwrap().success(), "harness failed");
    }

    let rings = ring_family(&ring_path);
    assert_eq!(rings.len(), 2, "one ring per process, got {rings:?}");
    for ring in &rings {
        let (committed, errors, dropped) = scan(ring);
        assert_eq!(errors, 0, "{ring:?} corrupted");
        assert_eq!(dropped, 0, "{ring:?} dropped frames");
        // The harness commits ENCODERS + 2 command buffers.
        assert_eq!(committed, ENCODERS + 2, "{ring:?} lost command buffers");
    }
}

/// A launcher that loaded the hook first — and touched Metal, as a Python
/// launcher does once the sidecar autoload imports mlx — must not keep its
/// child's events out of the recording.
#[test]
fn a_launcher_does_not_hide_its_childs_command_buffers() {
    let dylib = dylib_path();
    let tmpdir = tempfile::tempdir().unwrap();
    let ring_path = tmpdir.path().join("ring.bin");
    drop(create_ring(&ring_path, 1 << 22).unwrap());

    let status = harness(&dylib, &ring_path)
        .env("SMELTR_HARNESS_LAUNCHER", "1")
        .status()
        .unwrap();
    assert!(status.success(), "launcher failed: {status:?}");

    let committed: usize = ring_family(&ring_path).iter().map(|r| scan(r).0).sum();
    assert_eq!(
        committed, 2,
        "the child's command buffers never reached a ring"
    );
}

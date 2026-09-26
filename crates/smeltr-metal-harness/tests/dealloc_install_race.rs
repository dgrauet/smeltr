//! Threads making a process's first Metal allocations at the same instant
//! must not hang it (#264). The hook installs its dealloc swizzle lazily on
//! the first allocation; an unsynchronised check-then-install let two
//! threads both install, the second recording the replacement itself as
//! the "original" IMP — every later dealloc then called itself forever.
//!
//! `SMELTR_HOOK_TEST_INSTALL_DELAY_US` widens the window between the check
//! and the install so the race is deterministic instead of 1 run in 6.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use smeltr_metal_ring::create_ring;

fn dylib_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../metal-hook/build/libmetal_hook.dylib")
}

#[test]
fn concurrent_first_allocations_do_not_hang_the_process() {
    let dylib = dylib_path();
    assert!(
        dylib.exists(),
        "metal-hook dylib not built at {dylib:?}. Run `make -C metal-hook all` first.",
    );
    let tmpdir = tempfile::tempdir().unwrap();
    let ring_path = tmpdir.path().join("ring.bin");
    drop(create_ring(&ring_path, 1 << 20).unwrap());

    let mut child = Command::new(env!("CARGO_BIN_EXE_smeltr-metal-harness"))
        .env("DYLD_INSERT_LIBRARIES", &dylib)
        .env("SMELTR_RING_PATH", &ring_path)
        .env("SMELTR_HARNESS_RACE_THREADS", "8")
        .env("SMELTR_HOOK_TEST_INSTALL_DELAY_US", "50000")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let status = status.expect("traced process hung: dealloc swizzle installed twice");
    assert!(status.success(), "traced process failed: {status:?}");
}

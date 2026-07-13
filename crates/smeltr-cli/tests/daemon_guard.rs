//! The DaemonGuard invariant: no code path — including a panicking test —
//! can leave the guarded child process running.

mod common;

use common::DaemonGuard;
use std::process::{Command, Stdio};

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[test]
fn guard_kills_child_on_drop() {
    let child = Command::new("/bin/sleep")
        .arg("60")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let guard = DaemonGuard::new(child);
    assert!(alive(pid));
    drop(guard);
    assert!(!alive(pid), "guard must kill the child on drop");
}

#[test]
fn guard_kills_child_even_when_test_panics() {
    let pid = {
        let child = Command::new("/bin/sleep")
            .arg("60")
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let guard = DaemonGuard::new(child);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _hold = &guard;
            panic!("simulated mid-test failure");
        }));
        assert!(result.is_err());
        drop(guard);
        pid
    };
    assert!(!alive(pid), "child must not survive a panicking test");
}

#[test]
fn guard_stop_is_graceful_and_drop_is_then_a_noop() {
    let child = Command::new("/bin/sleep")
        .arg("60")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut guard = DaemonGuard::new(child);
    guard.stop();
    assert!(!alive(pid), "stop() must terminate the child");
    drop(guard); // must not panic or signal a reused pid
}

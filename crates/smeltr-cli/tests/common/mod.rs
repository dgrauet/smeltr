//! Shared e2e test helpers: RAII daemon guard so a panicking test can never
//! leak a running smeltrd (leaked daemons keep their global probes sampling,
//! load the machine, and cascade into socket-startup timeouts in later tests).
//!
//! Each integration-test binary compiles this module independently, so any
//! helper unused by one binary would trip dead_code there.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// Where assert_cmd/cargo put the compiled `smeltrd` next to the test binary.
pub fn smeltrd_path() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop(); // drop test name
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("smeltrd")
}

/// Generous, condition-based wait: returns as soon as the socket exists.
/// The long deadline costs nothing when healthy and absorbs full-workspace
/// parallel-test load.
pub fn wait_for_socket(path: &Path) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

/// RAII wrapper around a spawned daemon child process.
///
/// - `stop()` sends SIGTERM and waits, letting the daemon flush and finalize
///   sessions to disk (what tests previously did by hand).
/// - `Drop` force-kills the child if it is still alive — so a test that
///   panics mid-way can never leak a daemon.
pub struct DaemonGuard {
    child: Option<Child>,
}

impl DaemonGuard {
    /// Wraps an already-spawned child.
    pub fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    /// Spawns smeltrd with the given home/socket and waits for its socket.
    /// Panics (without leaking — the guard is constructed first) if the
    /// socket never appears, distinguishing a dead daemon (exit status in
    /// the message) from one that is merely too slow under load.
    pub fn spawn(home: &Path, sock: &Path) -> Self {
        let child = Command::new(smeltrd_path())
            .env("SMELTR_HOME", home)
            .env("SMELTR_SOCKET", sock)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn smeltrd");
        let mut guard = Self::new(child);
        if !wait_for_socket(sock) {
            let cause = match guard.child_mut().and_then(|c| c.try_wait().ok().flatten()) {
                Some(status) => format!("daemon exited during startup: {status}"),
                None => "daemon still alive but no socket after the deadline \
                         (machine overloaded?)"
                    .to_string(),
            };
            panic!("daemon never created its socket — {cause}");
        }
        guard
    }

    pub fn id(&self) -> u32 {
        self.child.as_ref().map(|c| c.id()).unwrap_or(0)
    }

    /// Direct access for tests that drive shutdown through the wire protocol
    /// and then poll `try_wait`. The guard still reaps on drop.
    pub fn child_mut(&mut self) -> Option<&mut Child> {
        self.child.as_mut()
    }

    /// Graceful shutdown: SIGTERM + wait, so the daemon flushes events and
    /// finalizes sessions to disk before the test reads them back.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = Command::new("kill")
                .arg("-TERM")
                .arg(child.id().to_string())
                .output();
            let _ = child.wait();
            // Small grace so metadata/socket cleanup lands on disk.
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

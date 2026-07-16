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
        // DIAGNOSTIC (issue #101, local-only, not for CI): when
        // SMELTR_TEST_DAEMON_LOG points at a directory, capture the daemon's
        // stdout+stderr (tracing writes to stdout) at debug level, and on a
        // socket-wait failure grab a `sample` stack snapshot of the still-
        // alive daemon before the guard kills it.
        let diag_dir = std::env::var("SMELTR_TEST_DAEMON_LOG").ok();
        let mut cmd = Command::new(smeltrd_path());
        cmd.env("SMELTR_HOME", home).env("SMELTR_SOCKET", sock);
        match &diag_dir {
            Some(dir) => {
                let tag = format!(
                    "{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0)
                );
                let out = std::fs::File::create(format!("{dir}/daemon-{tag}.out")).unwrap();
                let err = std::fs::File::create(format!("{dir}/daemon-{tag}.err")).unwrap();
                cmd.env("RUST_LOG", "debug")
                    .stdout(Stdio::from(out))
                    .stderr(Stdio::from(err));
            }
            None => {
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
        let child = cmd.spawn().expect("spawn smeltrd");
        let pid = child.id();
        let mut guard = Self::new(child);
        if !wait_for_socket(sock) {
            let cause = match guard.child_mut().and_then(|c| c.try_wait().ok().flatten()) {
                Some(status) => format!("daemon exited during startup: {status}"),
                None => {
                    if let Some(dir) = &diag_dir {
                        // Stack snapshot of the hung daemon (2 s sampling).
                        let _ = Command::new("sample")
                            .args([&pid.to_string(), "2", "-file"])
                            .arg(format!("{dir}/daemon-{pid}-hang.sample"))
                            .output();
                    }
                    "daemon still alive but no socket after the deadline \
                     (machine overloaded?)"
                        .to_string()
                }
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
            // SIGTERM first so the daemon kills its `log stream` child
            // (#158) and finalizes sessions; force-kill only if it does
            // not exit within the deadline.
            let _ = Command::new("kill")
                .arg("-TERM")
                .arg(child.id().to_string())
                .output();
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    _ => break,
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

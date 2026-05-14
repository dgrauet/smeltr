use crate::client::Client;
use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};
use std::path::PathBuf;
use std::process::Stdio;

/// Resolve the path to libmetal_hook.dylib for DYLD_INSERT_LIBRARIES.
///
/// Order (first hit wins):
///   1. `$SMELTR_DYLIB` — explicit dev override, lets you point at a
///      freshly-built dylib without reinstalling smeltr.
///   2. Embedded bytes extracted to `$TMPDIR/libmetal_hook-<fp>.dylib`
///      — the default path for end users.
///
/// Returns `None` only if both extraction fails AND no override is set
/// (i.e. genuine I/O failure on /tmp), which `run()` reports clearly.
fn resolve_dylib_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SMELTR_DYLIB") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return std::fs::canonicalize(pb).ok();
        }
    }
    match smeltr_cli::embedded_dylib::extract() {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("smeltr: failed to extract embedded dylib: {e}");
            None
        }
    }
}

fn is_hardened_binary(cmd: &str) -> bool {
    let out = std::process::Command::new("/usr/bin/codesign")
        .args(["--display", "--verbose=2", cmd])
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output();
    let Ok(out) = out else {
        return false;
    };
    let s = String::from_utf8_lossy(&out.stderr);
    // Apple's codesign prints "flags=0x10000(runtime)" for hardened binaries.
    s.contains("flags=") && s.contains("runtime")
}

/// Returns true if the target binary is arm64e-only (no plain arm64 slice).
/// Our metal-hook dylib is plain arm64; dyld refuses to inject across the
/// arm64 / arm64e boundary, so we must skip the hook when the target is
/// strict-arm64e. Most Apple-shipped system binaries (`/bin/sleep`,
/// `/usr/bin/whoami`, etc.) ship as arm64e + x86_64 with no arm64 slice.
fn is_arm64e_only_binary(cmd: &str) -> bool {
    let out = std::process::Command::new("/usr/bin/lipo")
        .args(["-archs", cmd])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    let Ok(out) = out else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let archs = String::from_utf8_lossy(&out.stdout);
    let has_arm64e = archs.split_whitespace().any(|a| a == "arm64e");
    let has_arm64 = archs.split_whitespace().any(|a| a == "arm64");
    has_arm64e && !has_arm64
}

fn smeltr_home() -> PathBuf {
    std::env::var_os("SMELTR_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").expect("HOME must be set");
            PathBuf::from(home).join(".smeltr")
        })
}

/// Spawn `cmd` with `args`, attach scoped probes (and optionally the Metal
/// hook) via the daemon, wait for it to exit, then detach probes with the
/// observed exit code. Returns the child's exit code (or -1 on signal).
pub async fn run(cmd: &str, args: &[String], no_hook: bool) -> anyhow::Result<i32> {
    let mut client = Client::connect().await?;

    let mut hook_decision: Option<(PathBuf, PathBuf)> = None;
    if !no_hook {
        if is_hardened_binary(cmd) {
            eprintln!(
                "smeltr: target binary `{cmd}` appears hardened; \
                 DYLD_INSERT_LIBRARIES will be stripped by SIP. \
                 Skipping Metal hook. Use brew Python to keep the hook, \
                 or pass --no-hook to silence this."
            );
        } else if is_arm64e_only_binary(cmd) {
            eprintln!(
                "smeltr: target binary `{cmd}` is arm64e-only; the metal-hook \
                 dylib is plain arm64. Skipping Metal hook. \
                 (Most Apple system binaries are arm64e; use a Homebrew Python \
                 or any arm64 binary to keep the hook.)"
            );
        } else if let Some(dylib) = resolve_dylib_path() {
            let rings_dir = smeltr_home().join("rings");
            std::fs::create_dir_all(&rings_dir)?;
            let ring_path = rings_dir.join(format!("{}.ring", uuid::Uuid::new_v4().simple()));
            smeltr_metal_ring::create_ring(&ring_path, 16 * 1024 * 1024)
                .map_err(|e| anyhow::anyhow!("create ring failed: {e}"))?;
            hook_decision = Some((dylib, ring_path));
        } else {
            eprintln!(
                "smeltr: could not provision metal-hook dylib (embedded extraction \
                 failed). Set SMELTR_DYLIB=/path/to/libmetal_hook.dylib to override. \
                 Continuing without hook."
            );
        }
    }

    let mut builder = std::process::Command::new(cmd);
    builder
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    if let Some((dylib, ring_path)) = &hook_decision {
        builder.env("DYLD_INSERT_LIBRARIES", dylib);
        builder.env("SMELTR_RING_PATH", ring_path);
    }

    let mut child = builder.spawn()?;
    let pid = child.id();

    // Attach scoped probes.
    let resp = client
        .request(ClientToDaemon::AttachScopedProbes { pid })
        .await?;
    if !matches!(resp, DaemonToClient::Ack) {
        let _ = child.kill();
        let _ = child.wait();
        if let Some((_, ring_path)) = &hook_decision {
            let _ = std::fs::remove_file(ring_path);
        }
        anyhow::bail!("daemon refused AttachScopedProbes: {resp:?}");
    }

    // Attach metal hook probe if applicable.
    if let Some((_, ring_path)) = &hook_decision {
        let resp = client
            .request(ClientToDaemon::AttachMetalHook {
                pid,
                ring_path: ring_path.to_string_lossy().into_owned(),
            })
            .await?;
        if !matches!(resp, DaemonToClient::Ack) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = client
                .request(ClientToDaemon::DetachScopedProbes {
                    pid,
                    exit_code: Some(-1),
                })
                .await;
            let _ = std::fs::remove_file(ring_path);
            anyhow::bail!("daemon refused AttachMetalHook: {resp:?}");
        }
    }

    // Wait for the child synchronously off the runtime worker.
    let status = tokio::task::spawn_blocking(move || child.wait()).await??;
    let exit_code = status.code().unwrap_or(-1);

    // Let the probe drain remaining frames from the ring.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    if hook_decision.is_some() {
        let _ = client
            .request(ClientToDaemon::DetachMetalHook { pid })
            .await;
    }
    let _ = client
        .request(ClientToDaemon::DetachScopedProbes {
            pid,
            exit_code: Some(exit_code),
        })
        .await;

    if let Some((_, ring_path)) = hook_decision {
        let _ = std::fs::remove_file(ring_path);
    }

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn resolve_prefers_env_override() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        std::env::set_var("SMELTR_DYLIB", &path);
        let resolved = resolve_dylib_path().expect("env override should win");
        assert_eq!(resolved, std::fs::canonicalize(&path).unwrap());
        std::env::remove_var("SMELTR_DYLIB");
    }

    #[test]
    #[serial_test::serial]
    fn resolve_falls_back_to_embedded_when_no_env() {
        std::env::remove_var("SMELTR_DYLIB");
        let resolved = resolve_dylib_path().expect("embedded must always resolve");
        assert!(resolved.exists());
        assert!(resolved.to_string_lossy().contains("libmetal_hook"));
    }
}

use crate::client::Client;
use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};
use std::path::PathBuf;
use std::process::Stdio;

fn dylib_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SMELTR_DYLIB") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return std::fs::canonicalize(pb).ok();
        }
    }
    let cwd = std::env::current_dir().ok()?;
    for rel in &[
        "metal-hook/build/libmetal_hook.dylib",
        "../metal-hook/build/libmetal_hook.dylib",
        "../../metal-hook/build/libmetal_hook.dylib",
    ] {
        let candidate = cwd.join(rel);
        if candidate.exists() {
            return std::fs::canonicalize(candidate).ok();
        }
    }
    None
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
        } else if let Some(dylib) = dylib_path() {
            let rings_dir = smeltr_home().join("rings");
            std::fs::create_dir_all(&rings_dir)?;
            let ring_path = rings_dir.join(format!("{}.ring", uuid::Uuid::new_v4().simple()));
            smeltr_metal_ring::create_ring(&ring_path, 16 * 1024 * 1024)
                .map_err(|e| anyhow::anyhow!("create ring failed: {e}"))?;
            hook_decision = Some((dylib, ring_path));
        } else {
            eprintln!(
                "smeltr: metal-hook dylib not found (set SMELTR_DYLIB or build \
                 with `make -C metal-hook`). Continuing without hook."
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

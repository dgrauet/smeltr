use crate::client::Client;
use smeltr_daemon::protocol::{ClientToDaemon, DaemonToClient};
use std::process::Stdio;

/// Spawn `cmd` with `args`, attach scoped probes for it via the daemon,
/// wait for it to exit, then detach probes with the observed exit code.
/// Returns the child's exit code (or -1 if it was killed by a signal).
pub async fn run(cmd: &str, args: &[String]) -> anyhow::Result<i32> {
    let mut client = Client::connect().await?;

    let mut child = std::process::Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()?;
    let pid = child.id();

    let resp = client
        .request(ClientToDaemon::AttachScopedProbes { pid })
        .await?;
    if !matches!(resp, DaemonToClient::Ack) {
        // Make sure we don't orphan the child if the daemon refuses.
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("daemon refused AttachScopedProbes: {resp:?}");
    }

    // Block on the child synchronously. The daemon is doing all the
    // event-driven work on its side; we just need the exit code.
    let status = tokio::task::spawn_blocking(move || child.wait()).await??;
    let exit_code = status.code().unwrap_or(-1);

    let resp = client
        .request(ClientToDaemon::DetachScopedProbes {
            pid,
            exit_code: Some(exit_code),
        })
        .await?;
    if !matches!(resp, DaemonToClient::Ack) {
        anyhow::bail!("daemon refused DetachScopedProbes: {resp:?}");
    }

    Ok(exit_code)
}

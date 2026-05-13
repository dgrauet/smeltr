use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum DaemonCmd {
    /// Spawn smeltrd in the background.
    Start,
    /// Send SIGTERM to the running smeltrd.
    Stop,
    /// Print PID, socket, sessions dir, and whether the socket responds.
    Status,
}

pub async fn run(cmd: DaemonCmd) -> anyhow::Result<()> {
    match cmd {
        DaemonCmd::Start => start().await,
        DaemonCmd::Stop => stop().await,
        DaemonCmd::Status => status().await,
    }
}

fn pid_file_path() -> PathBuf {
    let base = std::env::var("SMELTR_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var_os("HOME").expect("HOME must be set");
            PathBuf::from(home).join(".smeltr")
        });
    base.join("smeltrd.pid")
}

fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_file_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

async fn start() -> anyhow::Result<()> {
    if let Some(pid) = read_pid() {
        if process_alive(pid) {
            println!("smeltrd already running (pid {pid})");
            return Ok(());
        }
    }
    // Try ./target/debug/smeltrd first (dev), then $PATH.
    let exe = std::env::current_exe()?;
    let dev_path = exe.parent().map(|p| p.join("smeltrd"));
    let smeltrd = match dev_path {
        Some(p) if p.exists() => p,
        _ => PathBuf::from("smeltrd"),
    };
    let child = std::process::Command::new(&smeltrd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdin(std::process::Stdio::null())
        .spawn()?;
    // Give it a moment to write the pid file.
    for _ in 0..20 {
        if read_pid().map(process_alive).unwrap_or(false) {
            println!("smeltrd started (pid {})", child.id());
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!("smeltrd did not become healthy within 1s")
}

async fn stop() -> anyhow::Result<()> {
    let pid = read_pid().ok_or_else(|| anyhow::anyhow!("no pid file"))?;
    if !process_alive(pid) {
        anyhow::bail!("pid {pid} is not running");
    }
    unsafe {
        if libc_kill(pid as i32, 15) != 0 {
            anyhow::bail!("kill failed: {}", std::io::Error::last_os_error());
        }
    }
    for _ in 0..40 {
        if !process_alive(pid) {
            println!("smeltrd stopped");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!("smeltrd still alive after 2s")
}

async fn status() -> anyhow::Result<()> {
    match read_pid() {
        Some(pid) if process_alive(pid) => {
            println!("pid:    {pid}");
        }
        Some(pid) => println!("pid:    {pid} (stale, not running)"),
        None => println!("pid:    (no pid file)"),
    }
    println!("socket: {}", smeltr_daemon::server::socket_path().display());
    println!(
        "home:   {}",
        std::env::var("SMELTR_HOME").unwrap_or_else(|_| "$HOME/.smeltr".into())
    );
    Ok(())
}

fn process_alive(pid: u32) -> bool {
    unsafe { libc_kill(pid as i32, 0) == 0 }
}

// Minimal libc binding without pulling the libc crate.
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    kill(pid, sig)
}

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
    /// Install the LaunchAgent so smeltrd starts automatically at login.
    Install,
    /// Uninstall the LaunchAgent.
    Uninstall,
}

pub async fn run(cmd: DaemonCmd) -> anyhow::Result<()> {
    match cmd {
        DaemonCmd::Start => start().await,
        DaemonCmd::Stop => stop().await,
        DaemonCmd::Status => status().await,
        DaemonCmd::Install => install(),
        DaemonCmd::Uninstall => uninstall(),
    }
}

const LAUNCHAGENT_LABEL: &str = "com.smeltr.daemon";

fn home_dir() -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME env var not set")
}

fn launchagent_path_from_home(home: &str) -> PathBuf {
    PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHAGENT_LABEL}.plist"))
}

fn launchagent_path() -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    let home = home_dir()?;
    let home_str = home.to_str().context("HOME not utf-8")?;
    Ok(launchagent_path_from_home(home_str))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn plist_content(binary_path: &str, smeltr_home: &str) -> String {
    let bin = xml_escape(binary_path);
    let home = xml_escape(smeltr_home);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHAGENT_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{bin}</string>
        <string>--foreground</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>SMELTR_HOME</key>
        <string>{home}</string>
    </dict>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>ThrottleInterval</key>
    <integer>5</integer>
    <key>StandardOutPath</key>
    <string>{home}/smeltrd.log</string>
    <key>StandardErrorPath</key>
    <string>{home}/smeltrd.err</string>
</dict>
</plist>
"#
    )
}

pub fn install() -> anyhow::Result<()> {
    use anyhow::{anyhow, Context};

    let plist_path = launchagent_path()?;
    let dir = plist_path
        .parent()
        .context("LaunchAgents path has no parent")?;
    std::fs::create_dir_all(dir).context("create ~/Library/LaunchAgents")?;

    let me = std::env::current_exe().context("resolve current_exe")?;
    let smeltrd = me
        .parent()
        .context("current_exe has no parent")?
        .join("smeltrd");
    if !smeltrd.exists() {
        return Err(anyhow!(
            "smeltrd binary not found next to smeltr at {}; rebuild via `cargo build --workspace --release`",
            smeltrd.display()
        ));
    }
    let smeltrd_str = smeltrd.to_str().context("smeltrd path not utf-8")?;

    let home = home_dir()?;
    let smeltr_home = std::env::var("SMELTR_HOME")
        .unwrap_or_else(|_| home.join(".smeltr").to_string_lossy().into_owned());
    let _ = std::fs::create_dir_all(&smeltr_home);

    let plist = plist_content(smeltrd_str, &smeltr_home);
    std::fs::write(&plist_path, plist)
        .with_context(|| format!("write LaunchAgent plist to {}", plist_path.display()))?;
    println!("wrote LaunchAgent plist: {}", plist_path.display());

    let uid = unsafe { libc_getuid() };
    let target = format!("gui/{uid}");
    let bootstrap = std::process::Command::new("launchctl")
        .args(["bootstrap", &target])
        .arg(&plist_path)
        .status();
    let loaded = match bootstrap {
        Ok(s) if s.success() => true,
        _ => std::process::Command::new("launchctl")
            .arg("load")
            .arg(&plist_path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
    };

    if loaded {
        println!("launchctl loaded {LAUNCHAGENT_LABEL}");
        println!();
        println!("smeltrd will now start at every login and restart on crash.");
        println!("Verify with:  launchctl list | grep {LAUNCHAGENT_LABEL}");
        println!("Logs:         {smeltr_home}/smeltrd.log");
    } else {
        println!(
            "WARNING: plist was written but launchctl bootstrap/load failed.\n\
             Run manually: launchctl load {}",
            plist_path.display()
        );
    }
    Ok(())
}

pub fn uninstall() -> anyhow::Result<()> {
    use anyhow::Context;

    let plist_path = launchagent_path()?;
    if !plist_path.exists() {
        println!(
            "LaunchAgent not installed (no plist at {})",
            plist_path.display()
        );
        return Ok(());
    }

    let uid = unsafe { libc_getuid() };
    let target = format!("gui/{uid}/{LAUNCHAGENT_LABEL}");
    let bootout = std::process::Command::new("launchctl")
        .args(["bootout", &target])
        .status();
    let unloaded = match bootout {
        Ok(s) if s.success() => true,
        _ => std::process::Command::new("launchctl")
            .arg("unload")
            .arg(&plist_path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
    };
    if unloaded {
        println!("launchctl unloaded {LAUNCHAGENT_LABEL}");
    } else {
        println!(
            "WARNING: launchctl unload failed; removing plist anyway.\n\
             You may need to run:  launchctl bootout gui/{uid}/{LAUNCHAGENT_LABEL}"
        );
    }

    std::fs::remove_file(&plist_path)
        .with_context(|| format!("remove plist at {}", plist_path.display()))?;
    println!("removed {}", plist_path.display());
    Ok(())
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
    for _ in 0..50 {
        if !process_alive(pid) {
            println!("smeltrd stopped");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    anyhow::bail!("smeltrd still alive after 10s")
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
    fn getuid() -> u32;
}
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    kill(pid, sig)
}
unsafe fn libc_getuid() -> u32 {
    getuid()
}

#[cfg(test)]
mod install_tests {
    use super::*;

    #[test]
    fn plist_content_contains_required_fields() {
        let plist = plist_content("/Users/u/repo/target/release/smeltrd", "/Users/u/.smeltr");
        assert!(plist.starts_with("<?xml"));
        assert!(plist.contains("<!DOCTYPE plist"));
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains("<string>com.smeltr.daemon</string>"));
        assert!(plist.contains("/Users/u/repo/target/release/smeltrd"));
        assert!(plist.contains("--foreground"));
        assert!(plist.contains("<key>SMELTR_HOME</key>"));
        assert!(plist.contains("<string>/Users/u/.smeltr</string>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("smeltrd.log"));
        assert!(plist.contains("smeltrd.err"));
    }

    #[test]
    fn plist_escapes_xml_special_chars() {
        let plist = plist_content("/Users/u/repo & co/smeltrd", "/Users/u/<home>/.smeltr");
        assert!(plist.contains("&amp;"));
        assert!(plist.contains("&lt;"));
        assert!(plist.contains("&gt;"));
    }

    #[test]
    fn launchagent_path_uses_home() {
        let p = launchagent_path_from_home("/Users/u");
        assert_eq!(
            p,
            std::path::PathBuf::from("/Users/u/Library/LaunchAgents/com.smeltr.daemon.plist")
        );
    }
}

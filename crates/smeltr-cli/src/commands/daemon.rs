use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum DaemonCmd {
    /// Start smeltrd: through launchd when the LaunchAgent is installed,
    /// otherwise as a detached background process.
    Start,
    /// Stop smeltrd (via launchd when the LaunchAgent manages it).
    Stop,
    /// Stop then start smeltrd, e.g. to pick up a rebuilt binary.
    Restart,
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
        DaemonCmd::Restart => {
            stop().await?;
            start().await
        }
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

fn smeltr_home_dir() -> PathBuf {
    std::env::var("SMELTR_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var_os("HOME").expect("HOME must be set");
            PathBuf::from(home).join(".smeltr")
        })
}

fn pid_file_path() -> PathBuf {
    smeltr_home_dir().join("smeltrd.pid")
}

fn read_pid() -> Option<u32> {
    std::fs::read_to_string(pid_file_path())
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// How smeltrd is supervised for the current `SMELTR_HOME`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Supervision {
    /// No LaunchAgent serves this `SMELTR_HOME`: the CLI owns the daemon.
    Unmanaged,
    /// The LaunchAgent is installed for this `SMELTR_HOME`. `loaded` is
    /// whether launchd currently knows the job, `pid` the process it runs.
    LaunchAgent { loaded: bool, pid: Option<u32> },
}

#[derive(Debug, PartialEq, Eq)]
enum StartAction {
    SpawnDetached,
    Kickstart,
    Bootstrap,
}

/// Never spawn a detached smeltrd beside the LaunchAgent: it wins the
/// pid-file claim during launchd's `ThrottleInterval`, and launchd's own
/// instance then exits and is relaunched every few seconds for as long as
/// the stray daemon lives (days, in practice), whose output goes nowhere.
fn start_action(s: Supervision) -> StartAction {
    match s {
        Supervision::Unmanaged => StartAction::SpawnDetached,
        Supervision::LaunchAgent { loaded: true, .. } => StartAction::Kickstart,
        Supervision::LaunchAgent { loaded: false, .. } => StartAction::Bootstrap,
    }
}

/// A live pid-file daemon that the loaded LaunchAgent does not own.
fn outside_launchd(s: Supervision, live_pid: Option<u32>) -> Option<u32> {
    match s {
        Supervision::LaunchAgent { loaded: true, pid } if live_pid != pid => live_pid,
        _ => None,
    }
}

/// `SMELTR_HOME` the plist hands to smeltrd.
fn plist_smeltr_home(plist: &str) -> Option<String> {
    let after_key = &plist[plist.find("<key>SMELTR_HOME</key>")?..];
    let start = after_key.find("<string>")? + "<string>".len();
    let len = after_key[start..].find("</string>")?;
    Some(
        after_key[start..start + len]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&"),
    )
}

/// The job's pid from `launchctl print` (top-level `pid = N` line only;
/// nested blocks are indented deeper).
fn launchd_pid(print_output: &str) -> Option<u32> {
    print_output
        .lines()
        .find_map(|l| l.strip_prefix("\tpid = "))
        .and_then(|v| v.trim().parse().ok())
}

fn launchd_target() -> String {
    let uid = unsafe { libc_getuid() };
    format!("gui/{uid}/{LAUNCHAGENT_LABEL}")
}

fn supervision() -> Supervision {
    let Ok(plist_path) = launchagent_path() else {
        return Supervision::Unmanaged;
    };
    let Ok(plist) = std::fs::read_to_string(&plist_path) else {
        return Supervision::Unmanaged;
    };
    // A sandbox SMELTR_HOME gets its own CLI-owned daemon; only the home
    // the agent serves is launchd's to manage.
    match plist_smeltr_home(&plist) {
        Some(h) if std::path::Path::new(&h) == smeltr_home_dir() => {}
        _ => return Supervision::Unmanaged,
    }
    match std::process::Command::new("launchctl")
        .args(["print", &launchd_target()])
        .output()
    {
        Ok(out) if out.status.success() => Supervision::LaunchAgent {
            loaded: true,
            pid: launchd_pid(&String::from_utf8_lossy(&out.stdout)),
        },
        _ => Supervision::LaunchAgent {
            loaded: false,
            pid: None,
        },
    }
}

fn launchctl(args: &[&str]) -> anyhow::Result<()> {
    let status = std::process::Command::new("launchctl")
        .args(args)
        .status()?;
    anyhow::ensure!(
        status.success(),
        "launchctl {} failed: {status}",
        args.join(" ")
    );
    Ok(())
}

async fn start() -> anyhow::Result<()> {
    if let Some(pid) = live_pid() {
        println!("smeltrd already running (pid {pid})");
        return Ok(());
    }
    // Both launchd (plist) and the detached spawn below append the daemon's
    // output here; errors written after this offset are this start's.
    let err_path = smeltr_home_dir().join("smeltrd.err");
    let err_offset = std::fs::metadata(&err_path).map(|m| m.len()).unwrap_or(0);
    let mut child = match start_action(supervision()) {
        StartAction::Kickstart => {
            launchctl(&["kickstart", &launchd_target()])?;
            None
        }
        StartAction::Bootstrap => {
            let plist = launchagent_path()?;
            let uid = unsafe { libc_getuid() };
            launchctl(&["bootstrap", &format!("gui/{uid}"), &plist.to_string_lossy()])?;
            None
        }
        StartAction::SpawnDetached => {
            // Try ./target/debug/smeltrd first (dev), then $PATH.
            let exe = std::env::current_exe()?;
            let dev_path = exe.parent().map(|p| p.join("smeltrd"));
            let smeltrd = match dev_path {
                Some(p) if p.exists() => p,
                _ => PathBuf::from("smeltrd"),
            };
            std::fs::create_dir_all(smeltr_home_dir())?;
            let append = |p: PathBuf| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
            };
            Some(
                std::process::Command::new(&smeltrd)
                    .stdout(append(smeltr_home_dir().join("smeltrd.log"))?)
                    .stderr(append(err_path.clone())?)
                    .stdin(std::process::Stdio::null())
                    .spawn()?,
            )
        }
    };
    let launchd = child.is_none();
    // Healthy = the socket accepts a connection. The pid file alone is not
    // enough: smeltrd claims it before binding, so a daemon failing at bind
    // looked started (#236). The deadline covers launchd's ThrottleInterval
    // right after a stop, and a loaded machine.
    let sock = smeltr_daemon::server::socket_path();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if std::os::unix::net::UnixStream::connect(&sock).is_ok() {
            let pid = read_pid().map_or_else(|| "?".into(), |p| p.to_string());
            match launchd {
                false => println!("smeltrd started (pid {pid})"),
                true => println!("smeltrd started by launchd (pid {pid})"),
            }
            return Ok(());
        }
        if let Some(status) = child.as_mut().map(|c| c.try_wait()).transpose()?.flatten() {
            anyhow::bail!(
                "smeltrd exited ({status}) before serving {}{}",
                sock.display(),
                errors_since(&err_path, err_offset)
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    anyhow::bail!(
        "smeltrd did not serve {} within 30s{}",
        sock.display(),
        errors_since(&err_path, err_offset)
    )
}

/// What the daemon wrote to its stderr log past `offset`, formatted for an
/// error message (empty when nothing was written).
fn errors_since(err_path: &std::path::Path, offset: u64) -> String {
    use std::io::{Read, Seek};
    let mut text = String::new();
    let read = std::fs::File::open(err_path).and_then(|mut f| {
        f.seek(std::io::SeekFrom::Start(offset))?;
        f.read_to_string(&mut text)
    });
    match read {
        Ok(_) if !text.trim().is_empty() => {
            format!(":\n{}\n({})", text.trim_end(), err_path.display())
        }
        _ => String::new(),
    }
}

async fn stop() -> anyhow::Result<()> {
    // A bare SIGTERM is undone by KeepAlive: unload the job first. The plist
    // stays, so the agent comes back at next login or `smeltr daemon start`.
    if let Supervision::LaunchAgent { loaded: true, .. } = supervision() {
        launchctl(&["bootout", &launchd_target()])?;
        println!("launchctl booted out {LAUNCHAGENT_LABEL}");
    }
    // bootout waits for launchd's process; this also catches a daemon
    // running outside launchd.
    let Some(pid) = live_pid() else {
        println!("smeltrd stopped");
        return Ok(());
    };
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
        Some(pid) if live_pid() == Some(pid) => {
            println!("pid:    {pid}");
        }
        Some(pid) => println!("pid:    {pid} (stale, no smeltrd runs under it)"),
        None => println!("pid:    (no pid file)"),
    }
    let sup = supervision();
    match sup {
        Supervision::Unmanaged => println!("launchd: not managed"),
        Supervision::LaunchAgent { loaded: false, .. } => {
            println!("launchd: {LAUNCHAGENT_LABEL} installed, not loaded")
        }
        Supervision::LaunchAgent { pid: Some(p), .. } => {
            println!("launchd: {LAUNCHAGENT_LABEL} running (pid {p})")
        }
        Supervision::LaunchAgent { pid: None, .. } => {
            println!("launchd: {LAUNCHAGENT_LABEL} loaded, not running")
        }
    }
    if let Some(pid) = outside_launchd(sup, live_pid()) {
        println!(
            "WARNING: smeltrd pid {pid} runs outside launchd; launchd's instance \
             cannot start beside it and relaunches in a loop.\n         \
             Fix: smeltr daemon restart"
        );
    }
    println!("socket: {}", smeltr_daemon::server::socket_path().display());
    println!(
        "home:   {}",
        std::env::var("SMELTR_HOME").unwrap_or_else(|_| "$HOME/.smeltr".into())
    );
    Ok(())
}

/// The pid file's pid when a smeltrd runs under it. A bare liveness check
/// took a reused pid for the daemon after an unclean stop, and `stop` would
/// then signal that process (#242).
fn live_pid() -> Option<u32> {
    smeltr_daemon::recovery::live_daemon_pid(&pid_file_path())
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
    fn plist_smeltr_home_round_trips_through_plist_content() {
        let plist = plist_content("/bin/smeltrd", "/Users/u/a & b/.smeltr");
        assert_eq!(
            plist_smeltr_home(&plist).as_deref(),
            Some("/Users/u/a & b/.smeltr")
        );
    }

    #[test]
    fn plist_smeltr_home_absent_without_key() {
        assert_eq!(plist_smeltr_home("<plist><dict></dict></plist>"), None);
    }

    #[test]
    fn launchd_pid_reads_top_level_pid_only() {
        let print = "gui/501/com.smeltr.daemon = {\n\
                     \tstate = running\n\
                     \tendpoints = {\n\
                     \t\tpid = 999\n\
                     \t}\n\
                     \tpid = 76710\n\
                     }\n";
        assert_eq!(launchd_pid(print), Some(76710));
    }

    #[test]
    fn launchd_pid_none_when_not_running() {
        let print = "gui/501/com.smeltr.daemon = {\n\tstate = not running\n\truns = 3\n}\n";
        assert_eq!(launchd_pid(print), None);
    }

    #[test]
    fn start_never_spawns_beside_a_launch_agent() {
        // Spawning a detached smeltrd while the agent is installed is what
        // made launchd's instance lose the pid-file race and relaunch-loop.
        assert_eq!(
            start_action(Supervision::Unmanaged),
            StartAction::SpawnDetached
        );
        assert_eq!(
            start_action(Supervision::LaunchAgent {
                loaded: true,
                pid: None
            }),
            StartAction::Kickstart
        );
        assert_eq!(
            start_action(Supervision::LaunchAgent {
                loaded: false,
                pid: None
            }),
            StartAction::Bootstrap
        );
    }

    #[test]
    fn outside_launchd_flags_a_daemon_launchd_does_not_own() {
        let loaded = |pid| Supervision::LaunchAgent { loaded: true, pid };
        assert_eq!(outside_launchd(loaded(None), Some(82120)), Some(82120));
        assert_eq!(
            outside_launchd(loaded(Some(1277)), Some(82120)),
            Some(82120)
        );
        assert_eq!(outside_launchd(loaded(Some(1277)), Some(1277)), None);
        assert_eq!(outside_launchd(loaded(None), None), None);
        assert_eq!(outside_launchd(Supervision::Unmanaged, Some(82120)), None);
    }

    #[test]
    fn errors_since_reports_only_this_start() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("smeltrd.err");
        std::fs::write(&p, "Error: from an earlier start\n").unwrap();
        let offset = std::fs::metadata(&p).unwrap().len();
        assert_eq!(errors_since(&p, offset), "");
        std::fs::write(&p, "Error: from an earlier start\nError: bind failed\n").unwrap();
        let msg = errors_since(&p, offset);
        assert!(msg.contains("Error: bind failed"), "{msg}");
        assert!(!msg.contains("earlier"), "{msg}");
        assert_eq!(errors_since(&d.path().join("missing"), 0), "");
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

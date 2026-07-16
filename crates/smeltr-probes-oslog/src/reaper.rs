//! Boot-time reaper for leaked `log stream` children (#158).
//!
//! The oslog probe kills its `log stream` child on cancellation and sets
//! `kill_on_drop`, which covers every path where the daemon dies while
//! its tokio runtime is alive. But a SIGKILLed daemon (`launchctl
//! kickstart -k`, force-killed test daemons, the abort() in the panic
//! hook) never drops anything: the child is re-parented to launchd and
//! streams forever. Dozens of ephemeral daemons per dev day accumulated
//! 251 such orphans, pinning diagnosticd at ~60 % CPU.
//!
//! On startup the daemon calls [`reap_orphaned_streams`]: it lists
//! processes and SIGKILLs every `log stream` whose command line carries
//! OUR exact predicate (the predicate string is smeltr-specific, so this
//! cannot touch anyone else's streams) and whose parent is launchd
//! (ppid 1). Children of live smeltrds — including concurrent test
//! daemons — have their real parent pid and are left alone.

/// Parse `ps -axo pid=,ppid=,command=` output and return the pids of
/// orphaned (ppid 1) `log stream` processes carrying `predicate` in
/// their command line.
pub fn find_orphaned_streams(ps_output: &str, predicate: &str) -> Vec<u32> {
    let mut pids = Vec::new();
    for line in ps_output.lines() {
        let mut it = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (it.next(), it.next()) else {
            continue;
        };
        let (Ok(pid), Ok(ppid)) = (pid.parse::<u32>(), ppid.parse::<u32>()) else {
            continue;
        };
        if ppid != 1 {
            continue;
        }
        // ps prints argv space-separated; rejoining the remaining tokens
        // with single spaces matches the predicate string verbatim.
        let cmd = it.collect::<Vec<_>>().join(" ");
        if cmd.contains("log stream") && cmd.contains(predicate) {
            pids.push(pid);
        }
    }
    pids
}

/// Kill every orphaned smeltr `log stream` left behind by a previous
/// daemon that died without cleanup. Returns the number reaped. Never
/// fails: on any error (ps unavailable, kill refused) it just reaps less.
pub fn reap_orphaned_streams() -> usize {
    let Ok(out) = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
    else {
        return 0;
    };
    let ps_output = String::from_utf8_lossy(&out.stdout);
    let pids = find_orphaned_streams(&ps_output, &crate::parse::predicate());
    let mut reaped = 0;
    for pid in pids {
        if std::process::Command::new("/bin/kill")
            .args(["-9", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            reaped += 1;
        }
    }
    reaped
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREDICATE: &str =
        r#"(subsystem == "com.apple.gpurestart") OR (eventMessage CONTAINS "GPU watchdog")"#;

    fn stream_line(pid: u32, ppid: u32) -> String {
        format!("{pid:5} {ppid:5} /usr/bin/log stream --style ndjson --predicate {PREDICATE}")
    }

    #[test]
    fn finds_orphaned_streams_with_our_predicate() {
        let ps = format!(
            "{}\n{}\n{}\n",
            stream_line(100, 1),
            stream_line(200, 1),
            "  300     1 /usr/sbin/bluetoothd",
        );
        assert_eq!(find_orphaned_streams(&ps, PREDICATE), vec![100, 200]);
    }

    #[test]
    fn skips_streams_owned_by_a_live_daemon() {
        // ppid 4242 = a live smeltrd: not an orphan, keep it.
        let ps = stream_line(100, 4242);
        assert!(find_orphaned_streams(&ps, PREDICATE).is_empty());
    }

    #[test]
    fn skips_foreign_log_streams() {
        // Someone else's log stream, different predicate: never touch it.
        let ps = "  100     1 /usr/bin/log stream --predicate subsystem == \"com.example.app\"";
        assert!(find_orphaned_streams(ps, PREDICATE).is_empty());
    }

    #[test]
    fn skips_malformed_lines() {
        let ps = "garbage\n  abc   def /usr/bin/log stream\n\n";
        assert!(find_orphaned_streams(ps, PREDICATE).is_empty());
    }

    #[test]
    fn real_predicate_appears_verbatim_in_spawned_command_line() {
        // The reaper matches on the exact predicate() string; the probe
        // spawns `log stream` with that string as a single argv element,
        // which ps renders verbatim. Guard the coupling.
        let pred = crate::parse::predicate();
        let ps = format!("  100     1 /usr/bin/log stream --style ndjson --predicate {pred}");
        assert_eq!(find_orphaned_streams(&ps, &pred), vec![100]);
    }
}

use std::process::Command;

struct ProbeCheck {
    name: &'static str,
    status: Status,
    detail: String,
}

enum Status {
    Ok,
    Degraded,
    Failed,
}

impl Status {
    fn label(&self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Degraded => "degraded",
            Status::Failed => "failed",
        }
    }
}

fn check_vm() -> ProbeCheck {
    match smeltr_probes_vm::raw::read_sys() {
        Ok(_) => ProbeCheck {
            name: "vm",
            status: Status::Ok,
            detail: "host_statistics64 ok".into(),
        },
        Err(e) => ProbeCheck {
            name: "vm",
            status: Status::Failed,
            detail: e.to_string(),
        },
    }
}

fn check_proc() -> ProbeCheck {
    match smeltr_probes_proc::raw::read_sys() {
        Ok(v) if !v.is_empty() => ProbeCheck {
            name: "proc",
            status: Status::Ok,
            detail: format!("/usr/bin/top reachable, {} rows", v.len()),
        },
        Ok(_) => ProbeCheck {
            name: "proc",
            status: Status::Degraded,
            detail: "top returned 0 rows".into(),
        },
        Err(e) => ProbeCheck {
            name: "proc",
            status: Status::Failed,
            detail: e.to_string(),
        },
    }
}

fn check_thermal() -> ProbeCheck {
    match smeltr_probes_thermal::read_state() {
        Ok(level) => ProbeCheck {
            name: "thermal",
            status: Status::Degraded,
            detail: format!("kern.thermalstate={level} (root needed for SMC keys)"),
        },
        Err(e) => ProbeCheck {
            name: "thermal",
            status: Status::Degraded,
            detail: format!("kern.thermalstate unavailable: {e}"),
        },
    }
}

fn check_oslog() -> ProbeCheck {
    match Command::new("/usr/bin/log").arg("help").output() {
        Ok(o) if o.status.success() => ProbeCheck {
            name: "oslog",
            status: Status::Ok,
            detail: "/usr/bin/log available".into(),
        },
        Ok(_) | Err(_) => ProbeCheck {
            name: "oslog",
            status: Status::Failed,
            detail: "/usr/bin/log unavailable".into(),
        },
    }
}

fn check_ioreport() -> ProbeCheck {
    ProbeCheck {
        name: "ioreport",
        status: Status::Degraded,
        detail: "v1 stub: precise GPU residency requires metal-hook (Plan 3)".into(),
    }
}

fn check_crash_reports() -> ProbeCheck {
    let dir = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .map(|p| p.join("Library/Logs/DiagnosticReports"));
    match dir {
        Some(p) if p.exists() => ProbeCheck {
            name: "crash-reports",
            status: Status::Ok,
            detail: format!("watching {}", p.display()),
        },
        Some(p) => ProbeCheck {
            name: "crash-reports",
            status: Status::Degraded,
            detail: format!(
                "{} does not exist yet; will be created on first crash",
                p.display()
            ),
        },
        None => ProbeCheck {
            name: "crash-reports",
            status: Status::Failed,
            detail: "HOME not set".into(),
        },
    }
}

fn check_mach_exc() -> ProbeCheck {
    use smeltr_probes_mach_exceptions::port;
    if let Err(e) = port::install_for_pid(std::process::id()) {
        return ProbeCheck {
            name: "mach-exceptions",
            status: Status::Failed,
            detail: e.to_string(),
        };
    }
    // Self-observation always works; the realistic question is whether a
    // spawned child is observable. Hardened-runtime binaries (system and
    // Homebrew Python, all Apple platform binaries) refuse task_for_pid
    // even same-uid (#152), so probe an actual hardened child.
    match std::process::Command::new("/bin/sleep").arg("5").spawn() {
        Ok(mut child) => {
            let observable = port::can_observe_pid(child.id());
            let _ = child.kill();
            let _ = child.wait();
            match observable {
                Ok(()) => ProbeCheck {
                    name: "mach-exceptions",
                    status: Status::Ok,
                    detail: "task_for_pid works on spawned children; same-uid pids observable"
                        .into(),
                },
                Err(_) => ProbeCheck {
                    name: "mach-exceptions",
                    status: Status::Degraded,
                    detail: "task_for_pid(self) ok, but hardened children (system/Homebrew \
                             Python) are not observable — crash signals for those come from \
                             the crash-reports probe (.ips)"
                        .into(),
                },
            }
        }
        Err(_) => ProbeCheck {
            name: "mach-exceptions",
            status: Status::Ok,
            detail: "task_for_pid(self) succeeded; only same-uid pids are observable".into(),
        },
    }
}

pub fn run() -> anyhow::Result<()> {
    let checks = [
        check_vm(),
        check_proc(),
        check_thermal(),
        check_oslog(),
        check_ioreport(),
        check_crash_reports(),
        check_mach_exc(),
    ];
    println!("smeltr doctor — probe availability\n");
    for c in &checks {
        println!("  [{:<9}] {:<16} {}", c.status.label(), c.name, c.detail);
    }
    let failed = checks
        .iter()
        .filter(|c| matches!(c.status, Status::Failed))
        .count();
    if failed > 0 {
        println!("\n{failed} probe(s) failed. Smeltr will run with these disabled.");
    }
    Ok(())
}

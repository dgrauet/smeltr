//! Per-process `phys_footprint` reads through `proc_pid_rusage`.
//!
//! `ri_phys_footprint` is the metric jetsam decides kills on under macOS.
//! Neither `VmSample` (system-wide) nor `ProcTop` (CPU only) yields it, and
//! MTLDevice memory is not what the kernel looks at.

/// A process's memory footprint, as the kernel accounts for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footprint {
    /// `ri_phys_footprint` — current footprint in bytes.
    pub phys_bytes: u64,
    /// `ri_lifetime_max_phys_footprint` — maximum reached over the process's
    /// lifetime, in bytes.
    pub lifetime_max_bytes: u64,
}

/// Reads one PID's footprint. Returns `None` when the process is gone, cannot
/// be introspected, or the call fails for any other reason — observability
/// must never break the measurement under way.
#[cfg(target_os = "macos")]
pub fn read_footprint(pid: u32) -> Option<Footprint> {
    let mut ri = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: `ri` is a valid, aligned allocation of the size RUSAGE_INFO_V4
    // expects. libc types the buffer as `*mut *mut c_void` while the kernel
    // writes the struct itself into it, hence the cast.
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V4,
            ri.as_mut_ptr() as *mut *mut libc::c_void,
        )
    };
    if rc != 0 {
        return None;
    }
    // SAFETY: rc == 0 garantit que le noyau a rempli la struct.
    let ri = unsafe { ri.assume_init() };
    Some(Footprint {
        phys_bytes: ri.ri_phys_footprint,
        lifetime_max_bytes: ri.ri_lifetime_max_phys_footprint,
    })
}

#[cfg(not(target_os = "macos"))]
pub fn read_footprint(_pid: u32) -> Option<Footprint> {
    None
}

/// One process-table entry, reduced to what we need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcNode {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
}

#[cfg(target_os = "macos")]
mod sys {
    use super::ProcNode;
    use std::ffi::CStr;

    pub fn list_processes() -> Vec<ProcNode> {
        // First call: get the count of PIDs.
        // SAFETY: proc_listallpids accepts a null buffer to query the count.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return Vec::new();
        }

        // Allocate buffer with some headroom (the table can grow between calls).
        // Note: buffersize argument is in bytes, but return value is a pid count.
        let mut pids: Vec<libc::pid_t> = Vec::with_capacity(count as usize + 16);
        let buf_size = (pids.capacity() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;

        // Second call: fill the buffer with PIDs.
        // SAFETY: pids has the capacity we allocated, and buf_size matches that capacity in bytes.
        // IMPORTANT: proc_listallpids returns a PID COUNT, not a byte length.
        let pid_count =
            unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut libc::c_void, buf_size) };

        if pid_count <= 0 {
            return Vec::new();
        }

        let actual_count = (pid_count as usize).min(pids.capacity());
        // SAFETY: proc_listallpids just initialized actual_count entries, clamped to capacity.
        unsafe { pids.set_len(actual_count) };

        let mut result = Vec::new();
        for pid in pids {
            // Query process info for this PID.
            // SAFETY: proc_bsdinfo is a plain C struct of integers and byte
            // arrays, so the all-zero bit pattern is a valid inhabitant.
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            // SAFETY: info is a live, correctly sized proc_bsdinfo that
            // proc_pidinfo fills; the size argument matches the struct we
            // pass. The flavor PROC_PIDTBSDINFO and arg 0 are documented.
            let rc = unsafe {
                libc::proc_pidinfo(
                    pid,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    &mut info as *mut _ as *mut libc::c_void,
                    std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int,
                )
            };

            // Success is returning the size of the struct.
            if rc as usize != std::mem::size_of::<libc::proc_bsdinfo>() {
                // Process died between calls or other transient error—skip it.
                continue;
            }

            // Extract process name from the NUL-terminated c_char array.
            // SAFETY: pbi_comm is guaranteed to be a valid NUL-terminated C string or padded with nulls.
            let name = unsafe {
                CStr::from_ptr(info.pbi_comm.as_ptr())
                    .to_string_lossy()
                    .into_owned()
            };

            result.push(ProcNode {
                pid: info.pbi_pid,
                ppid: info.pbi_ppid,
                name,
            });
        }
        result
    }
}

/// Enumerates the process table through `libproc` (proc_listallpids +
/// proc_pidinfo).
///
/// Returns the processes the calling process can introspect. Processes owned
/// by other users or by root are omitted. That is intentional for a
/// user-launched probe: only the traced process and its descendants share the
/// same owner, and those are exactly what gets enumerated. Transient errors (a
/// process dying between the two calls) degrade silently: never a panic, never
/// a probe shutdown.
#[cfg(target_os = "macos")]
pub fn list_processes() -> Vec<ProcNode> {
    sys::list_processes()
}

#[cfg(not(target_os = "macos"))]
pub fn list_processes() -> Vec<ProcNode> {
    Vec::new()
}

/// Returns `root` and all its descendants, root in first position. Empty when
/// `root` is not in `all`.
pub fn descendants_of(root: u32, all: &[ProcNode]) -> Vec<ProcNode> {
    let Some(root_node) = all.iter().find(|n| n.pid == root) else {
        return Vec::new();
    };
    let mut out = vec![root_node.clone()];
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::from([root]);
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for child in all.iter().filter(|n| n.ppid == parent) {
            // `seen` bounds the walk: a ppid cycle manufactured by an
            // inconsistent snapshot must not loop forever.
            if seen.insert(child.pid) {
                out.push(child.clone());
                frontier.push(child.pid);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_own_footprint() {
        let f = read_footprint(std::process::id()).expect("own footprint readable");
        assert!(f.phys_bytes > 0, "phys_bytes = {}", f.phys_bytes);
        assert!(
            f.lifetime_max_bytes > 0,
            "lifetime_max_bytes = {}",
            f.lifetime_max_bytes
        );
        // `f.lifetime_max_bytes >= f.phys_bytes` does NOT hold in general and
        // must not be asserted here. The kernel updates
        // `ri_lifetime_max_phys_footprint` lazily, at page granularity, so it
        // can trail `ri_phys_footprint` by up to one page while another
        // thread/process is actively allocating. Reproduced 4/25 runs of
        // `cargo test -p smeltr-probes-proc` under concurrent allocation
        // (from `finds_spawned_children`), always with a delta of exactly
        // 16384 bytes (one page), e.g. "lifetime_max 1327416 < phys 1343800".
    }

    #[test]
    fn dead_pid_returns_none() {
        // PID 0 cannot be queried through proc_pid_rusage: this must degrade
        // cleanly rather than panic.
        assert!(read_footprint(0).is_none());
    }

    fn node(pid: u32, ppid: u32, name: &str) -> ProcNode {
        ProcNode {
            pid,
            ppid,
            name: name.into(),
        }
    }

    #[test]
    fn descendants_include_root_first() {
        let all = vec![
            node(1, 0, "launchd"),
            node(100, 1, "uv"),
            node(101, 100, "python"),
            node(102, 101, "worker"),
            node(200, 1, "unrelated"),
        ];
        let tree = descendants_of(100, &all);
        let pids: Vec<u32> = tree.iter().map(|n| n.pid).collect();
        assert_eq!(pids[0], 100, "la racine doit venir en premier");
        assert_eq!(pids.len(), 3);
        assert!(pids.contains(&101));
        assert!(pids.contains(&102));
        assert!(!pids.contains(&200));
    }

    #[test]
    fn descendants_of_leaf_is_just_itself() {
        let all = vec![node(1, 0, "launchd"), node(100, 1, "solo")];
        let tree = descendants_of(100, &all);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].pid, 100);
    }

    #[test]
    fn descendants_of_absent_root_is_empty() {
        let all = vec![node(1, 0, "launchd")];
        assert!(descendants_of(999, &all).is_empty());
    }

    /// A ppid cycle must not loop forever. It does not happen on a healthy
    /// system, but the enumeration runs over a non-atomic snapshot: two
    /// inconsistent reads can manufacture one.
    #[test]
    fn ppid_cycle_terminates() {
        let all = vec![node(100, 101, "a"), node(101, 100, "b")];
        let tree = descendants_of(100, &all);
        assert!(tree.len() <= 2, "infinite loop avoided");
    }

    #[test]
    fn lists_real_processes_including_self() {
        let all = list_processes();
        assert!(all.len() > 1, "expected > 1 process, got {}", all.len());
        let me = std::process::id();
        assert!(
            all.iter().any(|n| n.pid == me),
            "the test process must appear in the enumeration"
        );
    }

    #[test]
    fn finds_spawned_children() {
        use std::process::Command;
        use std::thread;
        use std::time::Duration;

        // Spawn three sleep children.
        let mut children = vec![];
        for _ in 0..3 {
            let child = Command::new("/bin/sleep")
                .arg("5")
                .spawn()
                .expect("spawn /bin/sleep");
            children.push(child);
        }

        // Give the kernel a moment to register them.
        thread::sleep(Duration::from_millis(50));

        // Enumerate processes and check all children are present, but defer
        // asserting so cleanup below always runs even on failure.
        let all = list_processes();
        let me = std::process::id();
        let child_pids: Vec<u32> = children.iter().map(|c| c.id()).collect();

        let mut failures: Vec<String> = Vec::new();
        for child_pid in &child_pids {
            match all.iter().find(|n| n.pid == *child_pid) {
                None => failures.push(format!(
                    "child pid {} not found in enumeration; all.len()={}",
                    child_pid,
                    all.len()
                )),
                Some(node) if node.ppid != me => failures.push(format!(
                    "child {} ppid {} != my pid {}",
                    child_pid, node.ppid, me
                )),
                Some(_) => {}
            }
        }

        // Clean up: kill and reap the children, even if failures were recorded.
        for mut child in children {
            let _ = child.kill();
            let _ = child.wait();
        }

        assert!(failures.is_empty(), "{}", failures.join("; "));
    }
}

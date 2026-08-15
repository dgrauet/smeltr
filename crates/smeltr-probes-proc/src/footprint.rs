//! Lecture de `phys_footprint` par processus via `proc_pid_rusage`.
//!
//! `ri_phys_footprint` est la métrique sur laquelle jetsam décide de tuer un
//! processus sous macOS. Ni `VmSample` (échelle système) ni `ProcTop` (CPU
//! seulement) ne la donnent, et la mémoire MTLDevice n'est pas ce que le
//! noyau regarde.

/// Empreinte mémoire d'un processus, telle que le noyau la comptabilise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footprint {
    /// `ri_phys_footprint` — empreinte courante en octets.
    pub phys_bytes: u64,
    /// `ri_lifetime_max_phys_footprint` — maximum atteint sur la vie du
    /// processus, en octets.
    pub lifetime_max_bytes: u64,
}

/// Lit l'empreinte d'un PID. Retourne `None` si le processus a disparu,
/// n'est pas interrogeable, ou si l'appel échoue pour toute autre raison —
/// l'observabilité ne doit jamais casser la mesure en cours.
#[cfg(target_os = "macos")]
pub fn read_footprint(pid: u32) -> Option<Footprint> {
    let mut ri = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
    // SAFETY: `ri` est une allocation valide et alignée de la taille attendue
    // pour RUSAGE_INFO_V4. libc type le buffer `*mut *mut c_void` alors que le
    // noyau y écrit la struct elle-même, d'où le cast.
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

/// Une entrée de la table des processus, réduite à ce dont on a besoin.
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
        let mut pids: Vec<libc::pid_t> = Vec::with_capacity(count as usize + 16);
        let buf_size = (pids.capacity() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;

        // Second call: fill the buffer with PIDs.
        // SAFETY: pids has the capacity we allocated, and buf_size matches that capacity in bytes.
        let bytes_written =
            unsafe { libc::proc_listallpids(pids.as_mut_ptr() as *mut libc::c_void, buf_size) };

        if bytes_written <= 0 {
            return Vec::new();
        }

        let actual_count = (bytes_written as usize) / std::mem::size_of::<libc::pid_t>();
        // SAFETY: proc_listallpids just initialized actual_count entries.
        unsafe { pids.set_len(actual_count) };

        let mut result = Vec::new();
        for pid in pids {
            // Query process info for this PID.
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            // SAFETY: info is a valid uninitialized proc_bsdinfo that we will fill via proc_pidinfo.
            // The flavor PROC_PIDTBSDINFO (3) and arg 0 are documented.
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

/// Énumère la table des processus via `sysctl(KERN_PROC_ALL)`.
///
/// Retourne un vecteur vide plutôt que d'échouer : une énumération ratée
/// doit dégrader la sonde, pas l'arrêter.
#[cfg(target_os = "macos")]
pub fn list_processes() -> Vec<ProcNode> {
    sys::list_processes()
}

#[cfg(not(target_os = "macos"))]
pub fn list_processes() -> Vec<ProcNode> {
    Vec::new()
}

/// Retourne `root` et toute sa descendance, racine en première position.
/// Vide si `root` n'est pas dans `all`.
pub fn descendants_of(root: u32, all: &[ProcNode]) -> Vec<ProcNode> {
    let Some(root_node) = all.iter().find(|n| n.pid == root) else {
        return Vec::new();
    };
    let mut out = vec![root_node.clone()];
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::from([root]);
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for child in all.iter().filter(|n| n.ppid == parent) {
            // `seen` borne le parcours : un cycle ppid fabriqué par un
            // instantané incohérent ne doit pas boucler.
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
            f.lifetime_max_bytes >= f.phys_bytes,
            "lifetime_max {} < phys {}",
            f.lifetime_max_bytes,
            f.phys_bytes
        );
    }

    #[test]
    fn dead_pid_returns_none() {
        // PID 0 n'est pas interrogeable via proc_pid_rusage : doit dégrader
        // proprement, pas paniquer.
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

    /// Un cycle ppid ne doit pas boucler à l'infini. Ça n'arrive pas sur un
    /// système sain, mais l'énumération est faite sur un instantané non
    /// atomique : deux lectures incohérentes peuvent en fabriquer un.
    #[test]
    fn ppid_cycle_terminates() {
        let all = vec![node(100, 101, "a"), node(101, 100, "b")];
        let tree = descendants_of(100, &all);
        assert!(tree.len() <= 2, "boucle infinie évitée");
    }

    #[test]
    fn lists_real_processes_including_self() {
        let all = list_processes();
        assert!(all.len() > 1, "attendu > 1 processus, eu {}", all.len());
        let me = std::process::id();
        assert!(
            all.iter().any(|n| n.pid == me),
            "le processus de test doit figurer dans l'énumération"
        );
    }
}

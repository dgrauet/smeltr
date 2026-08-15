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
#[allow(non_camel_case_types)]
mod sys {
    use super::ProcNode;

    // kinfo_proc structure size on modern macOS (roughly 648 bytes).
    // We don't need the exact layout, just enough to pass to sysctl.
    #[repr(C)]
    pub struct kinfo_proc {
        _data: [u8; 648],
    }

    /// Extract fields from a kinfo_proc buffer at known offsets.
    /// Based on Apple's bsd/sys/proc_internal.h and <sys/sysctl.h>.
    struct KinfoProcFields {
        pid: libc::pid_t,
        ppid: libc::pid_t,
        comm: [libc::c_char; 17],
    }

    impl KinfoProcFields {
        fn from_buffer(buf: &[u8]) -> Self {
            // p_pid is at offset 40 (after initial fields in struct proc)
            // SAFETY: buf is guaranteed to be at least 648 bytes from sysctl
            let pid =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().add(40) as *const libc::pid_t) };

            // p_comm is at offset 163 (char array of 17 bytes)
            let mut comm = [0i8; 17];
            // SAFETY: copying from known offset within our buffer
            unsafe {
                std::ptr::copy_nonoverlapping(
                    buf.as_ptr().add(163),
                    comm.as_mut_ptr() as *mut u8,
                    17,
                );
            }

            // e_ppid is at offset 192 + 32 = 224 (in the eproc part)
            // SAFETY: buf is large enough and e_ppid is at known offset
            let ppid =
                unsafe { std::ptr::read_unaligned(buf.as_ptr().add(224) as *const libc::pid_t) };

            KinfoProcFields { pid, ppid, comm }
        }
    }

    pub fn list_processes() -> Vec<ProcNode> {
        let mut mib: [libc::c_int; 4] = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL, 0];
        let mut len: libc::size_t = 0;

        // Premier appel : dimensionner le tampon.
        // SAFETY: mib est un tableau valide de 4 entiers ; buffer nul + len nul
        // est la forme documentée pour interroger la taille.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                std::ptr::null_mut(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || len == 0 {
            return Vec::new();
        }

        let entry_size = std::mem::size_of::<kinfo_proc>();
        // Marge : la table peut grandir entre les deux appels.
        let mut buf: Vec<u8> = vec![0; len + 16 * entry_size];
        let mut len2 = buf.len();
        // SAFETY: buf has capacity len2 ; le noyau écrit au plus
        // len2 octets et met à jour len2 avec ce qu'il a réellement écrit.
        let rc = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as libc::c_uint,
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut len2,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 {
            return Vec::new();
        }

        let count = len2 / entry_size;
        let mut result = Vec::new();
        for i in 0..count {
            let entry_buf = &buf[i * entry_size..(i + 1) * entry_size];
            let fields = KinfoProcFields::from_buffer(entry_buf);
            let raw: Vec<u8> = fields
                .comm
                .iter()
                .take_while(|c| **c != 0)
                .map(|c| *c as u8)
                .collect();
            result.push(ProcNode {
                pid: fields.pid as u32,
                ppid: fields.ppid as u32,
                name: String::from_utf8_lossy(&raw).into_owned(),
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

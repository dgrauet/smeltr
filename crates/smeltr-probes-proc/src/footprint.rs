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
}

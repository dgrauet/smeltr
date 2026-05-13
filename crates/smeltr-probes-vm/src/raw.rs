#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmRaw {
    pub wired_bytes: u64,
    pub active_bytes: u64,
    pub compressed_bytes: u64,
    pub swap_used_bytes: u64,
    pub page_outs: u64,
}

pub fn compute_rate(prev: &VmRaw, current: &VmRaw, dt_secs: f32) -> f32 {
    if dt_secs <= 0.0 {
        return 0.0;
    }
    let delta = current.page_outs.saturating_sub(prev.page_outs) as f32;
    delta / dt_secs
}

#[cfg(target_os = "macos")]
#[allow(non_camel_case_types)]
mod sys {
    use super::VmRaw;
    use mach2::kern_return::{kern_return_t, KERN_SUCCESS};
    use mach2::mach_types::host_t;
    use mach2::vm_types::{integer_t, natural_t};
    use std::mem;

    // mach2 0.4.3 does not expose `host_statistics64`, `HOST_VM_INFO64`, or
    // `vm_statistics64`. Declare them manually from Apple's
    // <mach/host_info.h> and <mach/vm_statistics.h>.
    const HOST_VM_INFO64: i32 = 4;

    type host_flavor_t = integer_t;
    type host_info64_t = *mut integer_t;
    type mach_msg_type_number_t = natural_t;

    #[repr(C)]
    #[derive(Copy, Clone, Default)]
    struct VmStatistics64 {
        free_count: natural_t,
        active_count: natural_t,
        inactive_count: natural_t,
        wire_count: natural_t,
        zero_fill_count: u64,
        reactivations: u64,
        pageins: u64,
        pageouts: u64,
        faults: u64,
        cow_faults: u64,
        lookups: u64,
        hits: u64,
        purges: u64,
        purgeable_count: natural_t,
        speculative_count: natural_t,
        decompressions: u64,
        compressions: u64,
        swapins: u64,
        swapouts: u64,
        compressor_page_count: natural_t,
        throttled_count: natural_t,
        external_page_count: natural_t,
        internal_page_count: natural_t,
        total_uncompressed_pages_in_compressor: u64,
    }

    extern "C" {
        fn mach_host_self() -> host_t;
        fn host_statistics64(
            host_priv: host_t,
            flavor: host_flavor_t,
            host_info_out: host_info64_t,
            host_info_outCnt: *mut mach_msg_type_number_t,
        ) -> kern_return_t;
    }

    pub fn read() -> std::io::Result<VmRaw> {
        unsafe {
            let host = mach_host_self();
            let mut stats: VmStatistics64 = mem::zeroed();
            let mut count = (mem::size_of::<VmStatistics64>() / mem::size_of::<integer_t>())
                as mach_msg_type_number_t;
            let kr = host_statistics64(
                host,
                HOST_VM_INFO64,
                &mut stats as *mut _ as host_info64_t,
                &mut count,
            );
            if kr != KERN_SUCCESS {
                return Err(std::io::Error::other(format!(
                    "host_statistics64 failed: {kr}"
                )));
            }
            let page_size = libc::sysconf(libc::_SC_PAGESIZE) as u64;
            Ok(VmRaw {
                wired_bytes: stats.wire_count as u64 * page_size,
                active_bytes: stats.active_count as u64 * page_size,
                compressed_bytes: stats.compressor_page_count as u64 * page_size,
                swap_used_bytes: read_swap_used().unwrap_or(0),
                page_outs: stats.pageouts,
            })
        }
    }

    fn read_swap_used() -> std::io::Result<u64> {
        #[repr(C)]
        struct XswUsage {
            xsu_total: u64,
            xsu_avail: u64,
            xsu_used: u64,
            xsu_pagesize: u32,
            xsu_encrypted: i32,
        }
        unsafe {
            let mut usage: XswUsage = std::mem::zeroed();
            let mut size = std::mem::size_of::<XswUsage>();
            let name = std::ffi::CString::new("vm.swapusage").unwrap();
            let rc = libc::sysctlbyname(
                name.as_ptr(),
                &mut usage as *mut _ as *mut _,
                &mut size,
                std::ptr::null_mut(),
                0,
            );
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(usage.xsu_used)
        }
    }
}

#[cfg(target_os = "macos")]
pub fn read_sys() -> std::io::Result<VmRaw> {
    sys::read()
}

#[cfg(not(target_os = "macos"))]
pub fn read_sys() -> std::io::Result<VmRaw> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "vm probe requires macOS",
    ))
}

#[cfg(all(test, target_os = "macos"))]
mod sys_tests {
    use super::*;

    #[test]
    fn read_sys_returns_nonzero_wired() {
        let r = read_sys().expect("host_statistics64 failed");
        assert!(
            r.wired_bytes > 1_000_000,
            "wired bytes suspiciously low: {}",
            r.wired_bytes
        );
    }
}

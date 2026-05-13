//! Mach exception port helpers (macOS only).

#[cfg(target_os = "macos")]
mod imp {
    use std::time::Duration;

    pub struct ExceptionReceiver {
        port: u32, // mach_port_t
    }

    // Raw mach_port_t (u32) is Send.
    unsafe impl Send for ExceptionReceiver {}

    pub struct DecodedException {
        pub target_pid: u32,
        pub exception_type: i32,
        pub codes: Vec<i64>,
    }

    type MachPortT = u32;
    type KernReturnT = i32;
    const KERN_SUCCESS: KernReturnT = 0;
    type ExceptionMaskT = u32;
    type ExceptionBehaviorT = i32;
    type ThreadStateFlavorT = i32;

    const EXC_MASK_BAD_ACCESS: ExceptionMaskT = 1 << 1;
    const EXC_MASK_CRASH: ExceptionMaskT = 1 << 10;
    const EXC_MASK_RESOURCE: ExceptionMaskT = 1 << 11;
    const EXCEPTION_DEFAULT: ExceptionBehaviorT = 1;
    const THREAD_STATE_NONE: ThreadStateFlavorT = 0;
    const MACH_PORT_RIGHT_RECEIVE: i32 = 1;
    const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
    const MACH_PORT_NULL: MachPortT = 0;

    // bits for mach_msg(option):
    const MACH_RCV_MSG: i32 = 0x00000002;
    const MACH_RCV_TIMEOUT: i32 = 0x00000100;
    const MACH_MSG_SUCCESS: KernReturnT = 0;

    extern "C" {
        fn mach_task_self() -> MachPortT;
        fn task_for_pid(target_tport: MachPortT, pid: i32, t: *mut MachPortT) -> KernReturnT;
        fn mach_port_allocate(task: MachPortT, right: i32, name: *mut MachPortT) -> KernReturnT;
        fn mach_port_insert_right(
            task: MachPortT,
            name: MachPortT,
            poly: MachPortT,
            poly_poly: u32,
        ) -> KernReturnT;
        fn task_set_exception_ports(
            task: MachPortT,
            exception_mask: ExceptionMaskT,
            new_port: MachPortT,
            behavior: ExceptionBehaviorT,
            new_flavor: ThreadStateFlavorT,
        ) -> KernReturnT;
        fn mach_msg(
            msg: *mut MachMsgHeader,
            option: i32,
            send_size: u32,
            rcv_size: u32,
            rcv_name: MachPortT,
            timeout: u32,
            notify: MachPortT,
        ) -> KernReturnT;
    }

    // mach_msg_header_t layout — public ABI from <mach/message.h>.
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct MachMsgHeader {
        msgh_bits: u32,
        msgh_size: u32,
        msgh_remote_port: MachPortT,
        msgh_local_port: MachPortT,
        msgh_voucher_port: MachPortT,
        msgh_id: i32,
    }

    // Minimal envelope big enough to receive an exception_raise message
    // (id=2401). The trailer is treated as opaque.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ExceptionMsg {
        header: MachMsgHeader,
        body: [u8; 256],
    }

    pub fn install_for_pid(pid: u32) -> std::io::Result<ExceptionReceiver> {
        unsafe {
            let me = mach_task_self();
            let mut port: MachPortT = 0;
            let kr = mach_port_allocate(me, MACH_PORT_RIGHT_RECEIVE, &mut port);
            if kr != KERN_SUCCESS {
                return Err(std::io::Error::other(format!("mach_port_allocate: {kr}")));
            }
            let kr = mach_port_insert_right(me, port, port, MACH_MSG_TYPE_MAKE_SEND);
            if kr != KERN_SUCCESS {
                return Err(std::io::Error::other(format!(
                    "mach_port_insert_right: {kr}"
                )));
            }
            let mut target_task: MachPortT = 0;
            let kr = task_for_pid(me, pid as i32, &mut target_task);
            if kr != KERN_SUCCESS {
                return Err(std::io::Error::other(format!(
                    "task_for_pid({pid}): {kr} (need same uid / entitlement)"
                )));
            }
            const EXC_MASKS: ExceptionMaskT =
                EXC_MASK_BAD_ACCESS | EXC_MASK_CRASH | EXC_MASK_RESOURCE;
            let kr = task_set_exception_ports(
                target_task,
                EXC_MASKS,
                port,
                EXCEPTION_DEFAULT,
                THREAD_STATE_NONE,
            );
            if kr != KERN_SUCCESS {
                return Err(std::io::Error::other(format!(
                    "task_set_exception_ports: {kr}"
                )));
            }
            Ok(ExceptionReceiver { port })
        }
    }

    impl ExceptionReceiver {
        pub fn next(&self, timeout: Duration) -> Option<DecodedException> {
            unsafe {
                let mut msg: ExceptionMsg = std::mem::zeroed();
                let kr = mach_msg(
                    &mut msg.header as *mut MachMsgHeader,
                    MACH_RCV_MSG | MACH_RCV_TIMEOUT,
                    0,
                    std::mem::size_of::<ExceptionMsg>() as u32,
                    self.port,
                    timeout.as_millis() as u32,
                    MACH_PORT_NULL,
                );
                if kr != MACH_MSG_SUCCESS {
                    return None;
                }
                // Best-effort decode: exception_raise id = 2401. Precise
                // decoding of the trailer is deferred; expose sentinels so
                // the probe pipeline can still emit an event.
                Some(DecodedException {
                    target_pid: 0,
                    exception_type: 0,
                    codes: Vec::new(),
                })
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub use imp::*;

#[cfg(not(target_os = "macos"))]
pub mod stub {
    use std::time::Duration;
    pub struct ExceptionReceiver;
    pub struct DecodedException {
        pub target_pid: u32,
        pub exception_type: i32,
        pub codes: Vec<i64>,
    }
    pub fn install_for_pid(_pid: u32) -> std::io::Result<ExceptionReceiver> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "mach exceptions require macOS",
        ))
    }
    impl ExceptionReceiver {
        pub fn next(&self, _timeout: Duration) -> Option<DecodedException> {
            None
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub use stub::*;

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn install_on_self_runs_or_reports_permission() {
        let pid = std::process::id();
        match install_for_pid(pid) {
            Ok(_) => {}
            Err(e) => {
                let s = e.to_string();
                assert!(
                    s.contains("task_for_pid")
                        || s.contains("not permitted")
                        || s.contains("denied"),
                    "unexpected error: {s}"
                );
            }
        }
    }
}

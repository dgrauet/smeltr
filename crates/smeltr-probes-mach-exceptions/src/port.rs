//! Mach exception port helpers (macOS only).

// ---- Pure parsing of mach_exception_raise / exception_raise messages ----
//
// Format (offsets on 64-bit Darwin; MIG messages are `#pragma pack(4)`,
// checked with offsetof against the SDK's __Request__mach_exception_raise_t):
//
//   0 | 24 | mach_msg_header_t (msgh_id is at offset 20)
//  24 |  4 | mach_msg_body_t.msgh_descriptor_count
//  28 | 12 | thread mach_msg_port_descriptor_t (port name at offset 28)
//  40 | 12 | task   mach_msg_port_descriptor_t (port name at offset 40)
//  52 |  8 | NDR_record_t
//  60 |  4 | exception_type_t (i32)
//  64 |  4 | mach_msg_type_number_t codeCnt
//  68 | 8N | int64_t code[N]  (msgid 2405)
//     | 4N | int32_t code[N]  (msgid 2401)
//
// mach_msg_port_descriptor_t is 12 bytes, not 16: a 16-byte assumption read
// the exception type from the low half of code[0] and never found the task
// port or the codes (#240).

const MACH_MSG_ID_EXCEPTION_RAISE: u32 = 2401;
const MACH_MSG_ID_MACH_EXCEPTION_RAISE: u32 = 2405;
const MAX_DECODED_CODES: usize = 8;

const HEADER_LEN: usize = 24;
const BODY_LEN: usize = 4;
const PORT_DESC_LEN: usize = 12;
const NDR_LEN: usize = 8;
const THREAD_PORT_OFFSET: usize = HEADER_LEN + BODY_LEN; // 28
const TASK_PORT_OFFSET: usize = THREAD_PORT_OFFSET + PORT_DESC_LEN; // 40
const EXC_TYPE_OFFSET: usize = TASK_PORT_OFFSET + PORT_DESC_LEN + NDR_LEN; // 60
const CODE_CNT_OFFSET: usize = EXC_TYPE_OFFSET + 4; // 64
const CODES_OFFSET: usize = CODE_CNT_OFFSET + 4; // 68

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedException {
    pub exception_type: i32,
    pub codes: Vec<i64>,
    pub thread_port: u32,
    pub task_port: u32,
}

/// Parses the bytes of a received mach_msg as exception_raise (2401) or
/// mach_exception_raise (2405). Returns None on truncation or wrong msgid.
pub fn parse_mach_exception_raise(buf: &[u8]) -> Option<ParsedException> {
    if buf.len() < CODES_OFFSET {
        return None;
    }
    let msgid = u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]);
    let is_64bit_codes = match msgid {
        MACH_MSG_ID_EXCEPTION_RAISE => false,
        MACH_MSG_ID_MACH_EXCEPTION_RAISE => true,
        _ => return None,
    };
    let thread_port = u32::from_le_bytes([
        buf[THREAD_PORT_OFFSET],
        buf[THREAD_PORT_OFFSET + 1],
        buf[THREAD_PORT_OFFSET + 2],
        buf[THREAD_PORT_OFFSET + 3],
    ]);
    let task_port = u32::from_le_bytes([
        buf[TASK_PORT_OFFSET],
        buf[TASK_PORT_OFFSET + 1],
        buf[TASK_PORT_OFFSET + 2],
        buf[TASK_PORT_OFFSET + 3],
    ]);
    let exception_type = i32::from_le_bytes([
        buf[EXC_TYPE_OFFSET],
        buf[EXC_TYPE_OFFSET + 1],
        buf[EXC_TYPE_OFFSET + 2],
        buf[EXC_TYPE_OFFSET + 3],
    ]);
    let raw_cnt = u32::from_le_bytes([
        buf[CODE_CNT_OFFSET],
        buf[CODE_CNT_OFFSET + 1],
        buf[CODE_CNT_OFFSET + 2],
        buf[CODE_CNT_OFFSET + 3],
    ]) as usize;
    let code_cnt = raw_cnt.min(MAX_DECODED_CODES);
    let code_size = if is_64bit_codes { 8 } else { 4 };
    let need = CODES_OFFSET + code_size * code_cnt;
    if buf.len() < need {
        return None;
    }
    let mut codes = Vec::with_capacity(code_cnt);
    for i in 0..code_cnt {
        let off = CODES_OFFSET + i * code_size;
        let v = if is_64bit_codes {
            i64::from_le_bytes([
                buf[off],
                buf[off + 1],
                buf[off + 2],
                buf[off + 3],
                buf[off + 4],
                buf[off + 5],
                buf[off + 6],
                buf[off + 7],
            ])
        } else {
            i32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]) as i64
        };
        codes.push(v);
    }
    Some(ParsedException {
        exception_type,
        codes,
        thread_port,
        task_port,
    })
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    fn synth_msg_64bit_codes() -> Vec<u8> {
        let mut buf = vec![0u8; 84];
        buf[20..24].copy_from_slice(&2405u32.to_le_bytes());
        buf[24..28].copy_from_slice(&2u32.to_le_bytes()); // descriptor_count
        buf[28..32].copy_from_slice(&0x1234u32.to_le_bytes()); // thread port
        buf[40..44].copy_from_slice(&0xabcdu32.to_le_bytes()); // task port
                                                               // NDR at 52 — zero
        buf[60..64].copy_from_slice(&1i32.to_le_bytes()); // EXC_BAD_ACCESS
        buf[64..68].copy_from_slice(&2u32.to_le_bytes()); // codeCnt
        buf[68..76].copy_from_slice(&1i64.to_le_bytes()); // KERN_INVALID_ADDRESS
        buf[76..84].copy_from_slice(&0xdead_beefi64.to_le_bytes()); // fault addr
        buf
    }

    #[test]
    fn parses_mach_exception_raise_with_two_codes() {
        let buf = synth_msg_64bit_codes();
        let p = parse_mach_exception_raise(&buf).expect("should parse");
        assert_eq!(p.exception_type, 1);
        assert_eq!(p.codes, vec![1i64, 0xdead_beef]);
        assert_eq!(p.thread_port, 0x1234);
        assert_eq!(p.task_port, 0xabcd);
    }

    #[test]
    fn rejects_truncated_buffer() {
        let buf = vec![0u8; 30];
        assert!(parse_mach_exception_raise(&buf).is_none());
    }

    #[test]
    fn rejects_unknown_msgid() {
        let mut buf = vec![0u8; 92];
        buf[20..24].copy_from_slice(&9999u32.to_le_bytes());
        assert!(parse_mach_exception_raise(&buf).is_none());
    }

    #[test]
    fn clamps_excessive_code_count() {
        let mut buf = vec![0u8; 92];
        buf[20..24].copy_from_slice(&2405u32.to_le_bytes());
        buf[64..68].copy_from_slice(&1000u32.to_le_bytes()); // wildly excessive
        let r = parse_mach_exception_raise(&buf);
        // Either rejected (buffer too short for 1000 codes) — most likely outcome.
        if let Some(p) = r {
            assert!(p.codes.len() <= MAX_DECODED_CODES);
        }
    }

    #[test]
    fn parses_legacy_exception_raise_32bit_codes() {
        let mut buf = vec![0u8; 92];
        buf[20..24].copy_from_slice(&2401u32.to_le_bytes());
        buf[64..68].copy_from_slice(&2u32.to_le_bytes());
        // Two 32-bit codes
        buf[68..72].copy_from_slice(&42i32.to_le_bytes());
        buf[72..76].copy_from_slice(&7i32.to_le_bytes());
        // exception
        buf[60..64].copy_from_slice(&10i32.to_le_bytes());
        let p = parse_mach_exception_raise(&buf).expect("should parse");
        assert_eq!(p.exception_type, 10);
        assert_eq!(p.codes, vec![42i64, 7i64]);
    }
}

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
    const MACH_EXCEPTION_CODES: ExceptionBehaviorT = 0x8000_0000_u32 as ExceptionBehaviorT;
    const THREAD_STATE_NONE: ThreadStateFlavorT = 0;
    const MACH_PORT_RIGHT_RECEIVE: i32 = 1;
    const MACH_MSG_TYPE_MAKE_SEND: u32 = 20;
    const MACH_PORT_NULL: MachPortT = 0;

    const KERN_FAILURE: KernReturnT = 5;
    const MACH_PORT_RIGHT_SEND: i32 = 0;

    // bits for mach_msg(option):
    const MACH_SEND_MSG: i32 = 0x00000001;
    const MACH_RCV_MSG: i32 = 0x00000002;
    const MACH_RCV_TIMEOUT: i32 = 0x00000100;
    const MACH_MSG_SUCCESS: KernReturnT = 0;

    extern "C" {
        fn mach_task_self() -> MachPortT;
        fn task_for_pid(target_tport: MachPortT, pid: i32, t: *mut MachPortT) -> KernReturnT;
        fn mach_port_deallocate(task: MachPortT, name: MachPortT) -> KernReturnT;
        fn mach_port_allocate(task: MachPortT, right: i32, name: *mut MachPortT) -> KernReturnT;
        fn mach_port_mod_refs(
            task: MachPortT,
            name: MachPortT,
            right: i32,
            delta: i32,
        ) -> KernReturnT;
        fn mach_msg_destroy(msg: *mut MachMsgHeader);
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
        // pid_for_task: libproc-style mach helper. Resolves a task port to its
        // owning unix PID. Returns 0 on success.
        fn pid_for_task(task: MachPortT, pid: *mut i32) -> i32;
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

    // __Reply__mach_exception_raise_t: header + NDR + RetCode.
    #[repr(C)]
    struct ExceptionReply {
        header: MachMsgHeader,
        ndr: [u8; 8],
        ret_code: KernReturnT,
    }

    // NDR_record: little-endian integers, ASCII chars, IEEE floats.
    const NDR_RECORD: [u8; 8] = [0, 0, 0, 0, 1, 0, 0, 0];
    const MACH_MSGH_BITS_REMOTE_MASK: u32 = 0x1f;
    // MIG replies carry the request id + 100.
    const MIG_REPLY_ID_OFFSET: i32 = 100;

    // Minimal envelope big enough to receive an exception_raise message
    // (id=2401). The trailer is treated as opaque.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ExceptionMsg {
        header: MachMsgHeader,
        body: [u8; 256],
    }

    /// Check whether this process may observe `pid` via `task_for_pid`,
    /// WITHOUT installing any exception port (#152). Hardened-runtime
    /// targets (system/Homebrew Python) refuse with kr=5 even same-uid,
    /// so `install_for_pid` on a real recorded child usually fails while
    /// a self-check succeeds.
    pub fn can_observe_pid(pid: u32) -> std::io::Result<()> {
        unsafe {
            let me = mach_task_self();
            let mut target: MachPortT = 0;
            let kr = task_for_pid(me, pid as i32, &mut target);
            if kr != KERN_SUCCESS {
                return Err(std::io::Error::other(format!(
                    "task_for_pid({pid}): {kr} (need same uid / entitlement)"
                )));
            }
            let _ = mach_port_deallocate(me, target);
            Ok(())
        }
    }

    pub fn install_for_pid(pid: u32) -> std::io::Result<ExceptionReceiver> {
        unsafe {
            let me = mach_task_self();
            let mut port: MachPortT = 0;
            let kr = mach_port_allocate(me, MACH_PORT_RIGHT_RECEIVE, &mut port);
            if kr != KERN_SUCCESS {
                return Err(std::io::Error::other(format!("mach_port_allocate: {kr}")));
            }
            // From here on, dropping the receiver releases the port on
            // every error path.
            let receiver = ExceptionReceiver { port };
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
                EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES,
                THREAD_STATE_NONE,
            );
            // The target keeps its own reference to our port; the task
            // right was only needed for the call.
            let _ = mach_port_deallocate(me, target_task);
            if kr != KERN_SUCCESS {
                return Err(std::io::Error::other(format!(
                    "task_set_exception_ports: {kr}"
                )));
            }
            Ok(receiver)
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
                let bytes = std::slice::from_raw_parts(
                    &msg as *const ExceptionMsg as *const u8,
                    std::mem::size_of::<ExceptionMsg>(),
                );
                let Some(parsed) = super::parse_mach_exception_raise(bytes) else {
                    // Not an exception message: release whatever rights it
                    // carries rather than leaking them.
                    mach_msg_destroy(&mut msg.header);
                    return None;
                };
                let mut target_pid: u32 = 0;
                let mut pid_out: i32 = 0;
                let kr = pid_for_task(parsed.task_port as MachPortT, &mut pid_out);
                if kr == KERN_SUCCESS && pid_out > 0 {
                    target_pid = pid_out as u32;
                }
                let me = mach_task_self();
                let _ = mach_port_deallocate(me, parsed.thread_port);
                let _ = mach_port_deallocate(me, parsed.task_port);
                reply_not_handled(&msg.header);
                Some(DecodedException {
                    target_pid,
                    exception_type: parsed.exception_type,
                    codes: parsed.codes,
                })
            }
        }
    }

    /// Answer the exception with KERN_FAILURE. The faulting thread blocks
    /// until its handler replies; "not handled" sends the exception on to
    /// the host handler (ReportCrash) and then the Unix signal, so the
    /// process crashes exactly as it would unobserved (#240).
    unsafe fn reply_not_handled(request: &MachMsgHeader) {
        let mut reply = ExceptionReply {
            header: MachMsgHeader {
                msgh_bits: request.msgh_bits & MACH_MSGH_BITS_REMOTE_MASK,
                msgh_size: std::mem::size_of::<ExceptionReply>() as u32,
                msgh_remote_port: request.msgh_remote_port,
                msgh_local_port: MACH_PORT_NULL,
                msgh_voucher_port: MACH_PORT_NULL,
                msgh_id: request.msgh_id + MIG_REPLY_ID_OFFSET,
            },
            ndr: NDR_RECORD,
            ret_code: KERN_FAILURE,
        };
        let kr = mach_msg(
            &mut reply.header,
            MACH_SEND_MSG,
            reply.header.msgh_size,
            0,
            MACH_PORT_NULL,
            0,
            MACH_PORT_NULL,
        );
        if kr != MACH_MSG_SUCCESS {
            tracing::warn!(kr, "mach exception reply failed");
        }
    }

    impl Drop for ExceptionReceiver {
        fn drop(&mut self) {
            unsafe {
                let me = mach_task_self();
                // Send first: once the receive right is gone the name turns
                // into a dead name and no longer holds a send right.
                let _ = mach_port_mod_refs(me, self.port, MACH_PORT_RIGHT_SEND, -1);
                let _ = mach_port_mod_refs(me, self.port, MACH_PORT_RIGHT_RECEIVE, -1);
            }
        }
    }

    #[cfg(test)]
    mod drop_tests {
        use super::*;

        extern "C" {
            fn mach_port_type(task: MachPortT, name: MachPortT, ptype: *mut u32) -> KernReturnT;
        }

        #[test]
        fn dropping_the_receiver_frees_its_port_name() {
            let receiver = install_for_pid(std::process::id()).unwrap();
            let port = receiver.port;
            drop(receiver);
            let mut ptype = 0u32;
            let kr = unsafe { mach_port_type(mach_task_self(), port, &mut ptype) };
            // KERN_INVALID_NAME (15): no right of any kind left under it.
            assert_eq!(kr, 15, "port name still holds rights 0x{ptype:x}");
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
    pub fn can_observe_pid(_pid: u32) -> std::io::Result<()> {
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
    use std::time::Duration;

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

    /// Set once the receiver thread has printed what it decoded.
    static DECODED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    /// Exit status of a fault that reached the Unix signal stage.
    const FAULT_HANDLED_EXIT: i32 = 42;

    /// Catches the SIGSEGV/SIGBUS the fault turns into once our handler has
    /// replied, and exits cleanly: a process killed by the signal would
    /// leave a real crash report — and the developer's daemon a post-mortem
    /// session — on every `cargo test` (#227).
    extern "C" fn exit_on_fault(_sig: i32) {
        // Atomic loads and nanosleep are async-signal-safe; give the
        // receiver thread up to 2 s to print before exiting.
        for _ in 0..200 {
            if DECODED.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            unsafe { libc::usleep(10_000) };
        }
        unsafe { libc::_exit(FAULT_HANDLED_EXIT) };
    }

    /// Child half of `real_fault_is_decoded_and_the_faulting_process_dies`.
    /// `task_for_pid` on another process is refused without root or an
    /// entitlement, but always allowed on oneself: the child watches its
    /// own task, prints what the receiver decoded, then faults.
    #[test]
    #[ignore = "helper, run by real_fault_is_decoded_and_the_faulting_process_dies"]
    fn fault_helper() {
        if std::env::var_os("SMELTR_MACH_EXC_FAULT").is_none() {
            return;
        }
        for sig in [libc::SIGSEGV, libc::SIGBUS] {
            unsafe { libc::signal(sig, exit_on_fault as *const () as libc::sighandler_t) };
        }
        let receiver = install_for_pid(std::process::id()).unwrap();
        std::thread::spawn(move || {
            if let Some(e) = receiver.next(Duration::from_secs(10)) {
                println!(
                    "decoded pid={} type={} codes={:?}",
                    e.target_pid, e.exception_type, e.codes
                );
                DECODED.store(true, std::sync::atomic::Ordering::Release);
            }
        });
        std::thread::sleep(Duration::from_millis(200));
        unsafe { std::ptr::read_volatile(16 as *const u8) };
    }

    #[test]
    fn real_fault_is_decoded_and_the_faulting_process_dies() {
        use std::io::Read;

        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "port::tests::fault_helper",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env("SMELTR_MACH_EXC_FAULT", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        // The faulting thread waits for the receiver's reply: without one it
        // never reaches the Unix signal stage and the process hangs.
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break Some(s);
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut out = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut out)
            .unwrap();
        let Some(status) = status else {
            panic!("faulting process must terminate after the exception; stdout: {out:?}");
        };
        assert_eq!(
            status.code(),
            Some(FAULT_HANDLED_EXIT),
            "the fault must reach the signal handler: {status:?}, stdout: {out:?}"
        );
        let want = format!("decoded pid={} type=1 codes=[1, 16]", child.id());
        assert!(
            out.contains(&want),
            "want {want:?} (EXC_BAD_ACCESS, KERN_INVALID_ADDRESS at 16), got {out:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn can_observe_pid_self_succeeds() {
        assert!(can_observe_pid(std::process::id()).is_ok());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn can_observe_pid_launchd_fails() {
        // pid 1 is root-owned launchd: task_for_pid must refuse.
        let err = can_observe_pid(1).unwrap_err();
        assert!(err.to_string().contains("task_for_pid(1)"), "{err}");
    }
}

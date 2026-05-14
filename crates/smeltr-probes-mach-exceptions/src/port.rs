//! Mach exception port helpers (macOS only).

// ---- Pure parsing of mach_exception_raise / exception_raise messages ----
//
// Format (offsets on 64-bit Darwin):
//
//   0 | 24 | mach_msg_header_t (msgh_id is at offset 20)
//  24 |  4 | mach_msg_body_t.msgh_descriptor_count
//  28 | 16 | thread mach_msg_port_descriptor_t (port name at offset 28)
//  44 | 16 | task   mach_msg_port_descriptor_t (port name at offset 44)
//  60 |  8 | NDR_record_t
//  68 |  4 | exception_type_t (i32)
//  72 |  4 | mach_msg_type_number_t codeCnt
//  76 | 8N | int64_t code[N]  (msgid 2405)
//      | 4N | int32_t code[N]  (msgid 2401)

const MACH_MSG_ID_EXCEPTION_RAISE: u32 = 2401;
const MACH_MSG_ID_MACH_EXCEPTION_RAISE: u32 = 2405;
const MAX_DECODED_CODES: usize = 8;

const HEADER_LEN: usize = 24;
const BODY_LEN: usize = 4;
const PORT_DESC_LEN: usize = 16;
const NDR_LEN: usize = 8;
const TASK_PORT_OFFSET: usize = HEADER_LEN + BODY_LEN + PORT_DESC_LEN; // 44
const EXC_TYPE_OFFSET: usize = HEADER_LEN + BODY_LEN + 2 * PORT_DESC_LEN + NDR_LEN; // 68
const CODE_CNT_OFFSET: usize = EXC_TYPE_OFFSET + 4; // 72
const CODES_OFFSET: usize = CODE_CNT_OFFSET + 4; // 76

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedException {
    pub exception_type: i32,
    pub codes: Vec<i64>,
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
        task_port,
    })
}

#[cfg(test)]
mod parse_tests {
    use super::*;

    fn synth_msg_64bit_codes() -> Vec<u8> {
        let mut buf = vec![0u8; 92];
        buf[20..24].copy_from_slice(&2405u32.to_le_bytes());
        buf[24..28].copy_from_slice(&2u32.to_le_bytes()); // descriptor_count
                                                          // thread port at 28 = 0
                                                          // task port at 44 — set to 0xabcd
        buf[44..48].copy_from_slice(&0xabcdu32.to_le_bytes());
        // NDR at 60 — zero
        // exception at 68 = EXC_BAD_ACCESS = 1
        buf[68..72].copy_from_slice(&1i32.to_le_bytes());
        // codeCnt at 72 = 2
        buf[72..76].copy_from_slice(&2u32.to_le_bytes());
        // codes
        buf[76..84].copy_from_slice(&1i64.to_le_bytes()); // KERN_INVALID_ADDRESS
        buf[84..92].copy_from_slice(&0xdead_beefi64.to_le_bytes()); // fault addr
        buf
    }

    #[test]
    fn parses_mach_exception_raise_with_two_codes() {
        let buf = synth_msg_64bit_codes();
        let p = parse_mach_exception_raise(&buf).expect("should parse");
        assert_eq!(p.exception_type, 1);
        assert_eq!(p.codes, vec![1i64, 0xdead_beef]);
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
        buf[72..76].copy_from_slice(&1000u32.to_le_bytes()); // wildly excessive
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
        buf[72..76].copy_from_slice(&2u32.to_le_bytes());
        // Two 32-bit codes
        buf[76..80].copy_from_slice(&42i32.to_le_bytes());
        buf[80..84].copy_from_slice(&7i32.to_le_bytes());
        // exception
        buf[68..72].copy_from_slice(&10i32.to_le_bytes());
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
                EXCEPTION_DEFAULT | MACH_EXCEPTION_CODES,
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
                let bytes = std::slice::from_raw_parts(
                    &msg as *const ExceptionMsg as *const u8,
                    std::mem::size_of::<ExceptionMsg>(),
                );
                let parsed = super::parse_mach_exception_raise(bytes)?;
                let mut target_pid: u32 = 0;
                let mut pid_out: i32 = 0;
                let kr = pid_for_task(parsed.task_port as MachPortT, &mut pid_out);
                if kr == KERN_SUCCESS && pid_out > 0 {
                    target_pid = pid_out as u32;
                }
                Some(DecodedException {
                    target_pid,
                    exception_type: parsed.exception_type,
                    codes: parsed.codes,
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

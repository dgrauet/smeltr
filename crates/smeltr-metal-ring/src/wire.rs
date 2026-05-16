//! Wire format mirroring metal-hook/include/smeltr_ring.h.

pub const RING_MAGIC: u32 = 0x534D_4C52; // "SMLR"
pub const RING_VERSION: u32 = 1;

pub mod kind {
    pub const PAD: u32 = 0;
    pub const CB_COMMITTED: u32 = 1;
    pub const CB_SCHEDULED: u32 = 2;
    pub const CB_COMPLETED: u32 = 3;
    pub const CB_WARNING: u32 = 4;
    pub const HEAP_ALLOC: u32 = 5;
    pub const HEAP_FREE: u32 = 6;
    pub const BUFFER_ALLOC: u32 = 7;
    pub const BUFFER_FREE: u32 = 8;
    pub const TEXTURE_ALLOC: u32 = 9;
    pub const TEXTURE_FREE: u32 = 10;
    pub const DROPPED: u32 = 11;
    pub const SKIPPED: u32 = 12;
    pub const CB_OPS: u32 = 13;
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RingHeader {
    pub magic: u32,
    pub version: u32,
    pub capacity: u64,
    pub head: u64,
    pub tail: u64,
    pub dropped: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    pub len: u32,
    pub kind: u32,
    pub ts_mono_ns: u64,
}

pub const RING_HEADER_BYTES: usize = 40;
pub const FRAME_HEADER_BYTES: usize = 16;

const _: () = {
    assert!(std::mem::size_of::<RingHeader>() == RING_HEADER_BYTES);
    assert!(std::mem::size_of::<FrameHeader>() == FRAME_HEADER_BYTES);
};

use crate::decode::{decode_frame, DecodedEvent};
use crate::error::RingError;
use crate::wire::{
    kind, FrameHeader, RingHeader, FRAME_HEADER_BYTES, RING_HEADER_BYTES, RING_MAGIC, RING_VERSION,
};
use memmap2::{MmapMut, MmapOptions};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct RingReader {
    mmap: MmapMut,
    capacity: u64,
    mask: u64,
}

pub fn open_for_read(path: &Path) -> Result<RingReader, RingError> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)?;
    let mmap = unsafe { MmapOptions::new().map_mut(&file)? };
    if mmap.len() < RING_HEADER_BYTES {
        return Err(RingError::Truncated(0));
    }
    let hdr = unsafe { *(mmap.as_ptr() as *const RingHeader) };
    if hdr.magic != RING_MAGIC {
        return Err(RingError::BadMagic(hdr.magic, RING_MAGIC));
    }
    if hdr.version != RING_VERSION {
        return Err(RingError::BadVersion(hdr.version, RING_VERSION));
    }
    if !hdr.capacity.is_power_of_two() {
        return Err(RingError::BadCapacity(hdr.capacity));
    }
    Ok(RingReader {
        mmap,
        capacity: hdr.capacity,
        mask: hdr.capacity - 1,
    })
}

impl RingReader {
    fn head_atomic(&self) -> &AtomicU64 {
        unsafe { &*(self.mmap.as_ptr().add(16) as *const AtomicU64) }
    }
    fn tail_atomic(&self) -> &AtomicU64 {
        unsafe { &*(self.mmap.as_ptr().add(24) as *const AtomicU64) }
    }
    fn data_ptr(&self) -> *const u8 {
        unsafe { self.mmap.as_ptr().add(RING_HEADER_BYTES) }
    }

    pub fn header_snapshot(&self) -> RingHeader {
        unsafe { *(self.mmap.as_ptr() as *const RingHeader) }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<DecodedEvent>, RingError> {
        loop {
            let head = self.head_atomic().load(Ordering::Acquire);
            let tail = self.tail_atomic().load(Ordering::Relaxed);
            if tail == head {
                return Ok(None);
            }
            let offset = (tail & self.mask) as usize;
            if offset + FRAME_HEADER_BYTES > self.capacity as usize {
                return Err(RingError::Truncated(tail));
            }
            let hdr = unsafe { *(self.data_ptr().add(offset) as *const FrameHeader) };
            if hdr.len < FRAME_HEADER_BYTES as u32 {
                return Err(RingError::Truncated(tail));
            }
            if hdr.kind == kind::PAD {
                self.tail_atomic()
                    .store(tail + hdr.len as u64, Ordering::Release);
                continue;
            }
            let payload_len = hdr.len as usize - FRAME_HEADER_BYTES;
            let payload_start = offset + FRAME_HEADER_BYTES;
            if payload_start + payload_len > self.capacity as usize {
                return Err(RingError::Truncated(tail));
            }
            let payload = unsafe {
                std::slice::from_raw_parts(self.data_ptr().add(payload_start), payload_len)
            };
            let frame = decode_frame(hdr.kind, payload)?;
            self.tail_atomic()
                .store(tail + hdr.len as u64, Ordering::Release);
            return Ok(Some(DecodedEvent {
                ts_mono_ns: hdr.ts_mono_ns,
                frame,
            }));
        }
    }
}

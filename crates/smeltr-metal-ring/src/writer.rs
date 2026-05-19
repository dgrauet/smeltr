use crate::error::RingError;
use crate::wire::{
    kind, FrameHeader, RingHeader, FRAME_HEADER_BYTES, RING_HEADER_BYTES, RING_MAGIC, RING_VERSION,
};
use memmap2::{MmapMut, MmapOptions};
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct RingWriter {
    mmap: MmapMut,
    capacity: u64,
    mask: u64,
}

pub fn create_ring(path: &Path, capacity_bytes: u64) -> Result<RingWriter, RingError> {
    if !capacity_bytes.is_power_of_two() || capacity_bytes < 64 {
        return Err(RingError::BadCapacity(capacity_bytes));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    let total = RING_HEADER_BYTES as u64 + capacity_bytes;
    file.set_len(total)?;
    let mut mmap = unsafe { MmapOptions::new().len(total as usize).map_mut(&file)? };

    let hdr = unsafe { &mut *(mmap.as_mut_ptr() as *mut RingHeader) };
    hdr.magic = RING_MAGIC;
    hdr.version = RING_VERSION;
    hdr.capacity = capacity_bytes;
    hdr.head = 0;
    hdr.tail = 0;
    hdr.dropped = 0;
    mmap.flush_range(0, RING_HEADER_BYTES)?;

    Ok(RingWriter {
        mmap,
        capacity: capacity_bytes,
        mask: capacity_bytes - 1,
    })
}

impl RingWriter {
    fn head_atomic(&self) -> &AtomicU64 {
        unsafe { &*(self.mmap.as_ptr().add(16) as *const AtomicU64) }
    }
    fn tail_atomic(&self) -> &AtomicU64 {
        unsafe { &*(self.mmap.as_ptr().add(24) as *const AtomicU64) }
    }
    fn dropped_atomic(&self) -> &AtomicU64 {
        unsafe { &*(self.mmap.as_ptr().add(32) as *const AtomicU64) }
    }
    fn data_mut(&mut self) -> *mut u8 {
        unsafe { self.mmap.as_mut_ptr().add(RING_HEADER_BYTES) }
    }

    fn write_frame(
        &mut self,
        kind_val: u32,
        ts_mono_ns: u64,
        payload: &[u8],
    ) -> Result<(), RingError> {
        let frame_len = (FRAME_HEADER_BYTES + payload.len() + 7) & !7usize;
        let frame_len64 = frame_len as u64;

        let tail = self.tail_atomic().load(Ordering::Acquire);
        let head = self.head_atomic().load(Ordering::Relaxed);
        let free = self.capacity - (head - tail);
        if frame_len64 > free {
            self.dropped_atomic().fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        let offset = (head & self.mask) as usize;
        let to_end = self.capacity as usize - offset;
        if frame_len > to_end {
            if to_end < FRAME_HEADER_BYTES {
                self.dropped_atomic().fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            let dst = unsafe { self.data_mut().add(offset) };
            let pad = FrameHeader {
                len: to_end as u32,
                kind: kind::PAD,
                ts_mono_ns,
            };
            unsafe {
                std::ptr::write(dst as *mut FrameHeader, pad);
            }
            self.head_atomic()
                .store(head + to_end as u64, Ordering::Release);
            return self.write_frame(kind_val, ts_mono_ns, payload);
        }

        let dst = unsafe { self.data_mut().add(offset) };
        let hdr = FrameHeader {
            len: frame_len as u32,
            kind: kind_val,
            ts_mono_ns,
        };
        unsafe {
            std::ptr::write(dst as *mut FrameHeader, hdr);
        }
        let payload_dst = unsafe { dst.add(FRAME_HEADER_BYTES) };
        unsafe {
            std::ptr::copy_nonoverlapping(payload.as_ptr(), payload_dst, payload.len());
        }
        if frame_len > FRAME_HEADER_BYTES + payload.len() {
            unsafe {
                std::ptr::write_bytes(
                    payload_dst.add(payload.len()),
                    0,
                    frame_len - FRAME_HEADER_BYTES - payload.len(),
                );
            }
        }
        self.head_atomic()
            .store(head + frame_len64, Ordering::Release);
        Ok(())
    }
}

fn push_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn push_i32(b: &mut Vec<u8>, v: i32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn push_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn push_i64(b: &mut Vec<u8>, v: i64) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn push_label(b: &mut Vec<u8>, label: Option<&str>) {
    match label {
        Some(s) => {
            push_u32(b, s.len() as u32);
            b.extend_from_slice(s.as_bytes());
        }
        None => {
            push_u32(b, 0);
        }
    }
}

impl RingWriter {
    pub fn write_cb_committed(
        &mut self,
        ts: u64,
        cb_id: u64,
        queue_id: u64,
        queue_depth: u32,
        label: Option<&str>,
    ) -> Result<(), RingError> {
        let mut p = Vec::with_capacity(32);
        push_u64(&mut p, cb_id);
        push_u64(&mut p, queue_id);
        push_u32(&mut p, queue_depth);
        push_label(&mut p, label);
        self.write_frame(kind::CB_COMMITTED, ts, &p)
    }
    pub fn write_cb_scheduled(
        &mut self,
        ts: u64,
        cb_id: u64,
        queue_id: u64,
    ) -> Result<(), RingError> {
        let mut p = Vec::with_capacity(16);
        push_u64(&mut p, cb_id);
        push_u64(&mut p, queue_id);
        self.write_frame(kind::CB_SCHEDULED, ts, &p)
    }
    #[allow(clippy::too_many_arguments)]
    pub fn write_cb_completed(
        &mut self,
        ts: u64,
        cb_id: u64,
        queue_id: u64,
        status: u32,
        error_code: Option<i64>,
        domain: Option<&str>,
        in_flight_ns: u64,
    ) -> Result<(), RingError> {
        let mut p = Vec::with_capacity(64);
        push_u64(&mut p, cb_id);
        push_u64(&mut p, queue_id);
        push_u32(&mut p, status);
        push_i32(&mut p, if error_code.is_some() { 1 } else { 0 });
        push_i64(&mut p, error_code.unwrap_or(0));
        push_label(&mut p, domain);
        push_u64(&mut p, in_flight_ns);
        self.write_frame(kind::CB_COMPLETED, ts, &p)
    }
    pub fn write_cb_warning(
        &mut self,
        ts: u64,
        cb_id: u64,
        queue_id: u64,
        elapsed: u64,
    ) -> Result<(), RingError> {
        let mut p = Vec::with_capacity(24);
        push_u64(&mut p, cb_id);
        push_u64(&mut p, queue_id);
        push_u64(&mut p, elapsed);
        self.write_frame(kind::CB_WARNING, ts, &p)
    }
    pub fn write_heap_alloc(
        &mut self,
        ts: u64,
        heap_id: u64,
        size: u64,
        label: Option<&str>,
    ) -> Result<(), RingError> {
        let mut p = Vec::new();
        push_u64(&mut p, heap_id);
        push_u64(&mut p, size);
        push_label(&mut p, label);
        self.write_frame(kind::HEAP_ALLOC, ts, &p)
    }
    pub fn write_heap_free(&mut self, ts: u64, heap_id: u64) -> Result<(), RingError> {
        let mut p = Vec::with_capacity(8);
        push_u64(&mut p, heap_id);
        self.write_frame(kind::HEAP_FREE, ts, &p)
    }
    pub fn write_buffer_alloc(
        &mut self,
        ts: u64,
        buffer_id: u64,
        heap_id: Option<u64>,
        size: u64,
        label: Option<&str>,
    ) -> Result<(), RingError> {
        let mut p = Vec::new();
        push_u64(&mut p, buffer_id);
        push_u32(&mut p, if heap_id.is_some() { 1 } else { 0 });
        push_u64(&mut p, heap_id.unwrap_or(0));
        push_u64(&mut p, size);
        push_label(&mut p, label);
        self.write_frame(kind::BUFFER_ALLOC, ts, &p)
    }
    pub fn write_buffer_free(&mut self, ts: u64, buffer_id: u64) -> Result<(), RingError> {
        let mut p = Vec::with_capacity(8);
        push_u64(&mut p, buffer_id);
        self.write_frame(kind::BUFFER_FREE, ts, &p)
    }
    pub fn write_texture_alloc(
        &mut self,
        ts: u64,
        texture_id: u64,
        heap_id: Option<u64>,
        size: u64,
        label: Option<&str>,
    ) -> Result<(), RingError> {
        let mut p = Vec::new();
        push_u64(&mut p, texture_id);
        push_u32(&mut p, if heap_id.is_some() { 1 } else { 0 });
        push_u64(&mut p, heap_id.unwrap_or(0));
        push_u64(&mut p, size);
        push_label(&mut p, label);
        self.write_frame(kind::TEXTURE_ALLOC, ts, &p)
    }
    pub fn write_texture_free(&mut self, ts: u64, texture_id: u64) -> Result<(), RingError> {
        let mut p = Vec::with_capacity(8);
        push_u64(&mut p, texture_id);
        self.write_frame(kind::TEXTURE_FREE, ts, &p)
    }
    pub fn write_cb_ops(
        &mut self,
        ts: u64,
        cb_id: u64,
        ops: &[(&str, Option<&str>, u64, u32)],
    ) -> Result<(), RingError> {
        let mut p = Vec::new();
        push_u64(&mut p, cb_id);
        push_u32(&mut p, ops.len() as u32);
        for (name, symbol, gpu_ns, count) in ops {
            push_u32(&mut p, name.len() as u32);
            p.extend_from_slice(name.as_bytes());
            match symbol {
                None => push_u32(&mut p, crate::wire::CB_OPS_SYMBOL_LEN_NONE),
                Some(s) => {
                    push_u32(&mut p, s.len() as u32);
                    p.extend_from_slice(s.as_bytes());
                }
            }
            push_u64(&mut p, *gpu_ns);
            push_u32(&mut p, *count);
        }
        self.write_frame(kind::CB_OPS, ts, &p)
    }
    pub fn write_device_mem_sample(
        &mut self,
        ts: u64,
        allocated_bytes: u64,
        recommended_max_bytes: u64,
        at_event: &str,
    ) -> Result<(), RingError> {
        let mut p = Vec::new();
        push_u64(&mut p, allocated_bytes);
        push_u64(&mut p, recommended_max_bytes);
        push_u32(&mut p, at_event.len() as u32);
        p.extend_from_slice(at_event.as_bytes());
        self.write_frame(kind::DEVICE_MEM_SAMPLE, ts, &p)
    }
}

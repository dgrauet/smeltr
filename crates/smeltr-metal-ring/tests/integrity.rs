//! Regression tests for ring integrity bugs found on the 2026-07-14 LTX-2
//! session (#113 investigation): a writer that wedged permanently at the
//! wrap point, and a reader that wedged permanently on a corrupt frame.

use smeltr_metal_ring::wire::{FRAME_HEADER_BYTES, RING_HEADER_BYTES};
use smeltr_metal_ring::{create_ring, open_for_read, DecodedFrame};
use std::io::{Read, Seek, SeekFrom, Write};

/// With 8-byte frame alignment, `head` could land 8 bytes before the wrap
/// boundary — too small for a PAD frame header — and the writer dropped
/// every subsequent frame forever without advancing. Frame alignment must
/// make that position unreachable: whatever the write sequence, draining
/// after each write must recover every frame with zero drops.
#[test]
fn writer_never_wedges_at_wrap_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ring.bin");
    let mut w = create_ring(&path, 64).unwrap();
    let mut r = open_for_read(&path).unwrap();

    let mut decoded = 0usize;
    for _ in 0..10 {
        // 24- and 32-byte frames under round8: head reaches offset 56,
        // leaving 8 bytes to the boundary and wedging the writer.
        w.write_buffer_free(1, 0xb).unwrap();
        w.write_cb_scheduled(2, 0x42, 0xa1).unwrap();
        while let Some(_ev) = r.next().unwrap() {
            decoded += 1;
        }
    }
    assert_eq!(decoded, 20, "every frame must survive the wrap");
    assert_eq!(
        r.header_snapshot().dropped,
        0,
        "no frame may be dropped by the wrap logic"
    );
}

/// Corrupt a frame's `kind` in place (as a torn concurrent write would).
fn corrupt_u32_at(path: &std::path::Path, file_offset: u64, value: u32) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    f.seek(SeekFrom::Start(file_offset)).unwrap();
    f.write_all(&value.to_le_bytes()).unwrap();
    f.sync_all().unwrap();
}

fn read_u32_at(path: &std::path::Path, file_offset: u64) -> u32 {
    let mut f = std::fs::File::open(path).unwrap();
    f.seek(SeekFrom::Start(file_offset)).unwrap();
    let mut buf = [0u8; 4];
    f.read_exact(&mut buf).unwrap();
    u32::from_le_bytes(buf)
}

/// A frame with an unknown kind (torn write) must be skipped using its
/// still-plausible length: the reader reports the error once, then decodes
/// the next frame instead of returning the same error forever.
#[test]
fn reader_skips_frame_with_unknown_kind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ring.bin");
    {
        let mut w = create_ring(&path, 1 << 16).unwrap();
        w.write_buffer_free(1, 0xaaaa).unwrap();
        w.write_buffer_free(2, 0xbbbb).unwrap();
    }
    // First frame starts at data offset 0; kind is at header offset +4.
    corrupt_u32_at(&path, (RING_HEADER_BYTES + 4) as u64, 0x6C69_6E00);

    let mut r = open_for_read(&path).unwrap();
    assert!(r.next().is_err(), "corrupt frame must surface an error");
    let ev = r
        .next()
        .expect("reader must have advanced past the corrupt frame")
        .expect("second frame must still be there");
    assert!(matches!(
        ev.frame,
        DecodedFrame::BufferFree { buffer_id: 0xbbbb }
    ));
    assert!(r.next().unwrap().is_none());
}

/// A frame whose length is implausible (torn header) cannot be skipped
/// precisely: the reader must resync to `head` (dropping the unread tail)
/// rather than spinning on the same bytes forever.
#[test]
fn reader_resyncs_to_head_on_implausible_len() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ring.bin");
    {
        let mut w = create_ring(&path, 1 << 16).unwrap();
        w.write_buffer_free(1, 0xaaaa).unwrap();
        w.write_buffer_free(2, 0xbbbb).unwrap();
    }
    // len of first frame -> garbage far beyond capacity.
    let orig_len = read_u32_at(&path, RING_HEADER_BYTES as u64);
    assert!(orig_len as usize >= FRAME_HEADER_BYTES);
    corrupt_u32_at(&path, RING_HEADER_BYTES as u64, 0xdead_beef);

    let mut r = open_for_read(&path).unwrap();
    assert!(r.next().is_err(), "corrupt frame must surface an error");
    // Unread data was abandoned, but the reader is live again.
    assert!(
        r.next().unwrap().is_none(),
        "reader must have resynced to head"
    );
}

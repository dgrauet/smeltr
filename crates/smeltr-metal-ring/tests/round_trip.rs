use smeltr_metal_ring::{create_ring, open_for_read, DecodedFrame};
use std::path::PathBuf;
use tempfile::tempdir;

fn tmp_ring(dir: &std::path::Path) -> PathBuf {
    dir.join("ring.bin")
}

#[test]
fn writer_then_reader_recovers_cb_lifecycle() {
    let dir = tempdir().unwrap();
    let path = tmp_ring(dir.path());

    {
        let mut w = create_ring(&path, 1 << 20).unwrap();
        w.write_cb_committed(1_000, 0x42, 0xa1, 3, Some("eval"))
            .unwrap();
        w.write_cb_scheduled(1_010, 0x42, 0xa1).unwrap();
        w.write_cb_completed(1_037, 0x42, 0xa1, 4, None, None, 10_270_000_000)
            .unwrap();
    }

    let mut r = open_for_read(&path).unwrap();
    let e1 = r.next().unwrap().unwrap();
    assert!(matches!(
        e1.frame,
        DecodedFrame::CbCommitted { cb_id: 0x42, queue_id: 0xa1, queue_depth: 3, ref label }
            if label.as_deref() == Some("eval")
    ));
    let e2 = r.next().unwrap().unwrap();
    assert!(matches!(
        e2.frame,
        DecodedFrame::CbScheduled {
            cb_id: 0x42,
            queue_id: 0xa1
        }
    ));
    let e3 = r.next().unwrap().unwrap();
    assert!(matches!(
        e3.frame,
        DecodedFrame::CbCompleted {
            cb_id: 0x42,
            status: 4,
            error_code: None,
            ..
        }
    ));
    assert!(r.next().unwrap().is_none());
}

#[test]
fn drops_when_full() {
    let dir = tempdir().unwrap();
    let path = tmp_ring(dir.path());
    let mut w = create_ring(&path, 128).unwrap(); // tiny

    for _ in 0..100 {
        let _ = w.write_buffer_alloc(0, 0xb, None, 4096, None);
    }
    drop(w);
    let r = open_for_read(&path).unwrap();
    let h = r.header_snapshot();
    assert!(
        h.dropped > 0,
        "expected drops to be recorded, got {}",
        h.dropped
    );
}

#[test]
fn pad_frame_wraps_around() {
    let dir = tempdir().unwrap();
    let path = tmp_ring(dir.path());
    {
        let mut w = create_ring(&path, 1 << 12).unwrap();
        for i in 0..1000 {
            let _ = w.write_cb_scheduled(i, i, i);
        }
    }
    let mut r = open_for_read(&path).unwrap();
    let mut n = 0;
    while r.next().unwrap().is_some() {
        n += 1;
    }
    assert!(n > 0, "reader saw zero events after wrapping writer");
}

#[test]
fn buffer_alloc_with_heap_round_trips() {
    let dir = tempdir().unwrap();
    let path = tmp_ring(dir.path());
    {
        let mut w = create_ring(&path, 1 << 16).unwrap();
        w.write_buffer_alloc(50, 0xb1, Some(0xa4), 8192, Some("video"))
            .unwrap();
        w.write_buffer_alloc(60, 0xb2, None, 4096, None).unwrap();
        w.write_buffer_free(70, 0xb1).unwrap();
    }
    let mut r = open_for_read(&path).unwrap();
    let e1 = r.next().unwrap().unwrap();
    assert!(matches!(
        e1.frame,
        DecodedFrame::BufferAlloc { buffer_id: 0xb1, heap_id: Some(0xa4), size_bytes: 8192, ref label }
            if label.as_deref() == Some("video")
    ));
    let e2 = r.next().unwrap().unwrap();
    assert!(matches!(
        e2.frame,
        DecodedFrame::BufferAlloc {
            buffer_id: 0xb2,
            heap_id: None,
            size_bytes: 4096,
            label: None
        }
    ));
    let e3 = r.next().unwrap().unwrap();
    assert!(matches!(
        e3.frame,
        DecodedFrame::BufferFree { buffer_id: 0xb1 }
    ));
}

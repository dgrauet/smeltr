use smeltr_metal_ring::{create_ring, open_for_read, DecodedFrame, DecodedOpSample};
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

#[test]
fn cb_ops_round_trips() {
    let dir = tempdir().unwrap();
    let path = tmp_ring(dir.path());
    {
        let mut w = create_ring(&path, 1 << 16).unwrap();
        w.write_cb_ops(
            1_000,
            0xdead_beef,
            &[
                ("Matmul", None, 6_200_000u64, 3u32),
                ("Softmax", None, 1_500_000u64, 1u32),
                ("RMSNorm", None, 400_000u64, 2u32),
            ],
        )
        .unwrap();
    }
    let mut r = open_for_read(&path).unwrap();
    let e = r.next().unwrap().unwrap();
    let ops = match e.frame {
        DecodedFrame::CbOps { cb_id, ops } => {
            assert_eq!(cb_id, 0xdead_beef);
            ops
        }
        other => panic!("expected CbOps, got {other:?}"),
    };
    assert_eq!(ops.len(), 3);
    assert_eq!(
        ops[0],
        DecodedOpSample {
            name: "Matmul".into(),
            symbol: None,
            gpu_ns: 6_200_000,
            count: 3
        }
    );
    assert_eq!(
        ops[1],
        DecodedOpSample {
            name: "Softmax".into(),
            symbol: None,
            gpu_ns: 1_500_000,
            count: 1
        }
    );
    assert_eq!(
        ops[2],
        DecodedOpSample {
            name: "RMSNorm".into(),
            symbol: None,
            gpu_ns: 400_000,
            count: 2
        }
    );
    assert!(r.next().unwrap().is_none());
}

#[test]
fn cb_ops_empty_round_trips() {
    let dir = tempdir().unwrap();
    let path = tmp_ring(dir.path());
    {
        let mut w = create_ring(&path, 1 << 12).unwrap();
        w.write_cb_ops(7, 42, &[]).unwrap();
    }
    let mut r = open_for_read(&path).unwrap();
    let e = r.next().unwrap().unwrap();
    match e.frame {
        DecodedFrame::CbOps { cb_id: 42, ops } => assert!(ops.is_empty()),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn cb_ops_long_name_round_trips() {
    let dir = tempdir().unwrap();
    let path = tmp_ring(dir.path());
    let long = "A".repeat(200);
    {
        let mut w = create_ring(&path, 1 << 16).unwrap();
        w.write_cb_ops(7, 42, &[(long.as_str(), None, 99, 1)])
            .unwrap();
    }
    let mut r = open_for_read(&path).unwrap();
    let e = r.next().unwrap().unwrap();
    match e.frame {
        DecodedFrame::CbOps { ops, .. } => {
            assert_eq!(ops.len(), 1);
            assert_eq!(ops[0].name.len(), 200);
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn cb_ops_round_trip_with_and_without_symbol() {
    let dir = tempdir().unwrap();
    let path = tmp_ring(dir.path());
    {
        let mut w = create_ring(&path, 1 << 16).unwrap();
        w.write_cb_ops(
            42,
            123,
            &[
                ("K_a", Some("gemm_t_n_bf16"), 1000u64, 5u32),
                ("K_b", None, 2000u64, 3u32),
            ],
        )
        .unwrap();
    }
    let mut r = open_for_read(&path).unwrap();
    let e = r.next().unwrap().unwrap();
    match e.frame {
        DecodedFrame::CbOps { cb_id, ops } => {
            assert_eq!(cb_id, 123);
            assert_eq!(ops.len(), 2);
            assert_eq!(ops[0].name, "K_a");
            assert_eq!(ops[0].symbol.as_deref(), Some("gemm_t_n_bf16"));
            assert_eq!(ops[0].gpu_ns, 1000);
            assert_eq!(ops[0].count, 5);
            assert_eq!(ops[1].name, "K_b");
            assert_eq!(ops[1].symbol, None);
            assert_eq!(ops[1].gpu_ns, 2000);
            assert_eq!(ops[1].count, 3);
        }
        other => panic!("expected CbOps, got {other:?}"),
    }
}

#[test]
fn device_mem_sample_round_trip() {
    let dir = tempdir().unwrap();
    let path = tmp_ring(dir.path());
    {
        let mut w = create_ring(&path, 64 * 1024).unwrap();
        w.write_device_mem_sample(42, 8_589_934_592, 17_179_869_184, "cb_committed")
            .unwrap();
    }
    let mut r = open_for_read(&path).unwrap();
    let e = r.next().unwrap().unwrap();
    match e.frame {
        DecodedFrame::DeviceMemSample {
            allocated_bytes,
            recommended_max_bytes,
            at_event,
        } => {
            assert_eq!(allocated_bytes, 8_589_934_592);
            assert_eq!(recommended_max_bytes, 17_179_869_184);
            assert_eq!(at_event, "cb_committed");
        }
        other => panic!("expected DeviceMemSample, got {other:?}"),
    }
}

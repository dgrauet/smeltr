use smeltr_core::event::Payload;
use smeltr_metal_ring::DecodedFrame;

pub fn frame_to_payload(f: DecodedFrame) -> Payload {
    match f {
        DecodedFrame::CbCommitted {
            cb_id,
            queue_id,
            queue_depth,
            label,
        } => Payload::MetalCbCommitted {
            cb_id,
            queue_id,
            queue_depth,
            label,
        },
        DecodedFrame::CbScheduled { cb_id, queue_id } => {
            Payload::MetalCbScheduled { cb_id, queue_id }
        }
        DecodedFrame::CbCompleted {
            cb_id,
            queue_id,
            status,
            error_code,
            error_domain,
            in_flight_ns,
        } => Payload::MetalCbCompleted {
            cb_id,
            queue_id,
            status,
            error_code,
            error_domain,
            in_flight_ns,
        },
        DecodedFrame::CbWarning {
            cb_id,
            queue_id,
            elapsed_ns,
        } => Payload::MetalCbWarning {
            cb_id,
            queue_id,
            elapsed_ns,
        },
        DecodedFrame::HeapAlloc {
            heap_id,
            size_bytes,
            label,
        } => Payload::MetalHeapAlloc {
            heap_id,
            size_bytes,
            label,
        },
        DecodedFrame::HeapFree { heap_id } => Payload::MetalHeapFree { heap_id },
        DecodedFrame::BufferAlloc {
            buffer_id,
            heap_id,
            size_bytes,
            label,
        } => Payload::MetalBufferAlloc {
            buffer_id,
            heap_id,
            size_bytes,
            label,
        },
        DecodedFrame::BufferFree { buffer_id } => Payload::MetalBufferFree { buffer_id },
        DecodedFrame::TextureAlloc {
            texture_id,
            heap_id,
            size_bytes,
            label,
        } => Payload::MetalTextureAlloc {
            texture_id,
            heap_id,
            size_bytes,
            label,
        },
        DecodedFrame::TextureFree { texture_id } => Payload::MetalTextureFree { texture_id },
        DecodedFrame::CbOps { cb_id, ops } => Payload::MetalCbOps {
            cb_id,
            ops: ops
                .into_iter()
                .map(|o| smeltr_core::event::OpSample {
                    name: o.name,
                    symbol: None,
                    gpu_ns: o.gpu_ns,
                    count: o.count,
                })
                .collect(),
        },
        DecodedFrame::Dropped { count } => Payload::MetalHookDropped { count },
        DecodedFrame::Skipped { reason } => Payload::MetalHookSkipped { reason },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_metal_ring::DecodedOpSample;

    #[test]
    fn maps_cb_committed() {
        let p = frame_to_payload(DecodedFrame::CbCommitted {
            cb_id: 1,
            queue_id: 2,
            queue_depth: 3,
            label: Some("x".into()),
        });
        assert!(matches!(
            p,
            Payload::MetalCbCommitted {
                cb_id: 1,
                queue_id: 2,
                queue_depth: 3,
                ..
            }
        ));
    }

    #[test]
    fn maps_cb_completed_with_error() {
        let p = frame_to_payload(DecodedFrame::CbCompleted {
            cb_id: 1,
            queue_id: 2,
            status: 5,
            error_code: Some(0x0e),
            error_domain: Some("MTLCommandBufferErrorDomain".into()),
            in_flight_ns: 1000,
        });
        assert!(matches!(
            p,
            Payload::MetalCbCompleted {
                error_code: Some(0x0e),
                ..
            }
        ));
    }

    #[test]
    fn maps_dropped_to_hook_dropped() {
        let p = frame_to_payload(DecodedFrame::Dropped { count: 42 });
        assert!(matches!(p, Payload::MetalHookDropped { count: 42 }));
    }

    #[test]
    fn cb_ops_translates_with_ops() {
        let p = frame_to_payload(DecodedFrame::CbOps {
            cb_id: 0xdead_beef,
            ops: vec![
                DecodedOpSample {
                    name: "Matmul".into(),
                    gpu_ns: 6_200_000,
                    count: 3,
                },
                DecodedOpSample {
                    name: "Softmax".into(),
                    gpu_ns: 1_500_000,
                    count: 1,
                },
            ],
        });
        match p {
            Payload::MetalCbOps { cb_id, ops } => {
                assert_eq!(cb_id, 0xdead_beef);
                assert_eq!(ops.len(), 2);
                assert_eq!(ops[0].name, "Matmul");
                assert_eq!(ops[0].gpu_ns, 6_200_000);
                assert_eq!(ops[0].count, 3);
            }
            other => panic!("expected MetalCbOps, got {other:?}"),
        }
    }

    #[test]
    fn cb_ops_translates_empty() {
        let p = frame_to_payload(DecodedFrame::CbOps {
            cb_id: 1,
            ops: vec![],
        });
        match p {
            Payload::MetalCbOps { cb_id: 1, ops } => assert!(ops.is_empty()),
            other => panic!("expected MetalCbOps, got {other:?}"),
        }
    }
}

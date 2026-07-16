#[cfg(target_os = "macos")]
fn main() {
    use metal::*;
    let device = Device::system_default().expect("no Metal device");
    let queue = device.new_command_queue();

    // Buffer alloc — should produce a BufferAlloc event.
    let buf = device.new_buffer(4096, MTLResourceOptions::StorageModeShared);
    buf.set_label("smeltr-harness-buffer");

    // Alloc + release probe: the drop below MUST reach the buffer's dealloc
    // (BufferFree frame). Guards against the hook retaining app buffers —
    // an ARC method-family mismatch in the new* swizzles once added one
    // phantom retain per buffer, keeping every MLX-released buffer alive.
    {
        let free_probe = device.new_buffer(8192, MTLResourceOptions::StorageModeShared);
        free_probe.set_label("smeltr-harness-free-probe");
    }

    // No-op command buffer with a blit encoder.
    {
        let cb = queue.new_command_buffer();
        cb.set_label("smeltr-harness-noop");
        let encoder = cb.new_blit_command_encoder();
        encoder.end_encoding();
        cb.commit();
        cb.wait_until_completed();
    }

    // Compile a tiny compute kernel literally named `gemm_test_kernel`, build a
    // compute pipeline state, and dispatch it once. This exercises the
    // PSO-creation swizzle (which records the function name into the PSO map)
    // and the op-aggregation loop (which looks the name up and writes it into
    // the CB_OPS frame).
    let msl_source = r#"
        #include <metal_stdlib>
        using namespace metal;
        kernel void gemm_test_kernel(device float *out [[buffer(0)]],
                                     uint gid [[thread_position_in_grid]]) {
            out[gid] = (float)gid;
        }
    "#;
    let library = device
        .new_library_with_source(msl_source, &CompileOptions::new())
        .expect("library compile failed");
    let function = library
        .get_function("gemm_test_kernel", None)
        .expect("missing function");
    let pso = device
        .new_compute_pipeline_state_with_function(&function)
        .expect("PSO creation failed");

    let out_buf = device.new_buffer(4096, MTLResourceOptions::StorageModeShared);
    out_buf.set_label("smeltr-harness-gemm-out");

    let cb = queue.new_command_buffer();
    cb.set_label("smeltr-harness-gemm");
    let encoder = cb.new_compute_command_encoder();
    encoder.set_compute_pipeline_state(&pso);
    encoder.set_buffer(0, Some(&out_buf), 0);
    encoder.dispatch_threads(MTLSize::new(16, 1, 1), MTLSize::new(16, 1, 1));
    encoder.end_encoding();
    cb.commit();
    cb.wait_until_completed();

    // Optional loop mode for e2e tests that need many encoders spread over
    // time (e.g. the stage-sampling backoff-retry test). Each iteration goes
    // through computeCommandEncoderWithDispatchType: — the path the hook
    // substitutes for stage-boundary sampling.
    let iters: u32 = std::env::var("SMELTR_HARNESS_ENCODERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let sleep_ms: u64 = std::env::var("SMELTR_HARNESS_SLEEP_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // SMELTR_HARNESS_NO_WAIT=1: commit all loop CBs back-to-back without
    // waiting, so they queue behind each other (deep in-flight pipeline);
    // wait once at the end. SMELTR_HARNESS_HEAVY=1: use a compute-bound
    // kernel (~tens of ms per CB) so queueing is measurable.
    let no_wait = std::env::var("SMELTR_HARNESS_NO_WAIT").as_deref() == Ok("1");
    let heavy = std::env::var("SMELTR_HARNESS_HEAVY").as_deref() == Ok("1");
    let loop_pso = if heavy {
        let busy_src = r#"
            #include <metal_stdlib>
            using namespace metal;
            kernel void busy_kernel(device float *out [[buffer(0)]],
                                    uint gid [[thread_position_in_grid]]) {
                float acc = (float)gid;
                for (uint i = 0; i < 200000; ++i) {
                    acc = acc * 1.0000001f + 0.5f;
                }
                out[gid % 1024] = acc;
            }
        "#;
        let lib = device
            .new_library_with_source(busy_src, &CompileOptions::new())
            .expect("busy kernel compile failed");
        let f = lib
            .get_function("busy_kernel", None)
            .expect("missing busy_kernel");
        device
            .new_compute_pipeline_state_with_function(&f)
            .expect("busy PSO creation failed")
    } else {
        pso.clone()
    };
    let mut last_cb = None;
    for _ in 0..iters {
        let cb = queue.new_command_buffer();
        let encoder = cb.compute_command_encoder_with_dispatch_type(MTLDispatchType::Serial);
        encoder.set_compute_pipeline_state(&loop_pso);
        encoder.set_buffer(0, Some(&out_buf), 0);
        let threads = if heavy { 32768 } else { 16 };
        encoder.dispatch_threads(MTLSize::new(threads, 1, 1), MTLSize::new(16, 1, 1));
        encoder.end_encoding();
        cb.commit();
        if no_wait {
            last_cb = Some(cb.to_owned());
        } else {
            cb.wait_until_completed();
        }
        if sleep_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
        }
    }
    if let Some(cb) = last_cb {
        cb.wait_until_completed();
    }

    // Let the hook flush completion handlers before exit.
    std::thread::sleep(std::time::Duration::from_millis(750));
    println!("harness done");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("smeltr-metal-harness only runs on macOS");
    std::process::exit(1);
}

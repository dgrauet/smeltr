// A command-line tool must link CoreGraphics for `MTLCreateSystemDefaultDevice`
// to return a device: without it the call returns nil on some machines,
// the paravirtualized GPU of the CI runners among them. metal-rs linked it
// implicitly; objc2-metal does not.
#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {}

#[cfg(target_os = "macos")]
fn main() {
    use objc2_foundation::NSString;
    use objc2_metal::*;
    // SMELTR_HARNESS_LAUNCHER=1: behave like a Python launcher under
    // `smeltr record` (the sidecar autoload imports mlx, which allocates a
    // Metal heap): allocate one buffer, then run this same harness as a
    // child (which inherits the hook and the ring) and wait for it.
    if std::env::var("SMELTR_HARNESS_LAUNCHER").as_deref() == Ok("1") {
        let device = MTLCreateSystemDefaultDevice().expect("no Metal device");
        let _held = device.newBufferWithLength_options(4096, MTLResourceOptions::StorageModeShared);
        let exe = std::env::current_exe().expect("current exe");
        let status = std::process::Command::new(exe)
            .env_remove("SMELTR_HARNESS_LAUNCHER")
            .status()
            .expect("spawn child harness");
        std::process::exit(status.code().unwrap_or(1));
    }

    let device = MTLCreateSystemDefaultDevice().expect("no Metal device");

    // SMELTR_HARNESS_RACE_THREADS=<n>: n threads make the process's first
    // buffer allocations at the same instant, then exit. The hook installs
    // its dealloc swizzle lazily on the first allocation, so this is the
    // install race (#264).
    if let Some(n) = std::env::var("SMELTR_HARNESS_RACE_THREADS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        let gate = std::sync::Arc::new(std::sync::Barrier::new(n));
        let workers: Vec<_> = (0..n)
            .map(|_| {
                let device = device.clone();
                let gate = gate.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    for _ in 0..4 {
                        drop(device.newBufferWithLength_options(
                            4096,
                            MTLResourceOptions::StorageModeShared,
                        ));
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().expect("race worker panicked");
        }
        println!("race done");
        return;
    }

    let queue = device.newCommandQueue().expect("command queue");

    // Buffer alloc — should produce a BufferAlloc event.
    let buf = device
        .newBufferWithLength_options(4096, MTLResourceOptions::StorageModeShared)
        .expect("buffer alloc");
    buf.setLabel(Some(&NSString::from_str("smeltr-harness-buffer")));

    // Alloc + release probe: the drop below MUST reach the buffer's dealloc
    // (BufferFree frame). Guards against the hook retaining app buffers —
    // an ARC method-family mismatch in the new* swizzles once added one
    // phantom retain per buffer, keeping every MLX-released buffer alive.
    {
        let free_probe = device
            .newBufferWithLength_options(8192, MTLResourceOptions::StorageModeShared)
            .expect("buffer alloc");
        free_probe.setLabel(Some(&NSString::from_str("smeltr-harness-free-probe")));
    }

    // SMELTR_HARNESS_LABEL=<text>: label the no-op command buffer with an
    // arbitrary (e.g. very long, multibyte) string — the hook copies labels
    // into fixed-size frame buffers.
    let noop_label =
        std::env::var("SMELTR_HARNESS_LABEL").unwrap_or_else(|_| "smeltr-harness-noop".to_string());

    // No-op command buffer with a blit encoder.
    {
        let cb = queue.commandBuffer().expect("command buffer");
        cb.setLabel(Some(&NSString::from_str(&noop_label)));
        let encoder = cb.blitCommandEncoder().expect("blit encoder");
        encoder.endEncoding();
        cb.commit();
        cb.waitUntilCompleted();
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
        .newLibraryWithSource_options_error(
            &NSString::from_str(msl_source),
            Some(&MTLCompileOptions::new()),
        )
        .expect("library compile failed");
    let function = library
        .newFunctionWithName(&NSString::from_str("gemm_test_kernel"))
        .expect("missing function");
    let pso = device
        .newComputePipelineStateWithFunction_error(&function)
        .expect("PSO creation failed");

    let out_buf = device
        .newBufferWithLength_options(4096, MTLResourceOptions::StorageModeShared)
        .expect("buffer alloc");
    out_buf.setLabel(Some(&NSString::from_str("smeltr-harness-gemm-out")));

    let cb = queue.commandBuffer().expect("command buffer");
    cb.setLabel(Some(&NSString::from_str("smeltr-harness-gemm")));
    let encoder = cb.computeCommandEncoder().expect("compute encoder");
    encoder.setComputePipelineState(&pso);
    // SAFETY: out_buf is a live 4096-byte buffer; the kernel writes 16 floats.
    unsafe { encoder.setBuffer_offset_atIndex(Some(&out_buf), 0, 0) };
    encoder.dispatchThreads_threadsPerThreadgroup(size(16), size(16));
    encoder.endEncoding();
    cb.commit();
    cb.waitUntilCompleted();

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
            .newLibraryWithSource_options_error(
                &NSString::from_str(busy_src),
                Some(&MTLCompileOptions::new()),
            )
            .expect("busy kernel compile failed");
        let f = lib
            .newFunctionWithName(&NSString::from_str("busy_kernel"))
            .expect("missing busy_kernel");
        device
            .newComputePipelineStateWithFunction_error(&f)
            .expect("busy PSO creation failed")
    } else {
        pso.clone()
    };
    let mut last_cb = None;
    for _ in 0..iters {
        let cb = queue.commandBuffer().expect("command buffer");
        let encoder = cb
            .computeCommandEncoderWithDispatchType(MTLDispatchType::Serial)
            .expect("compute encoder");
        encoder.setComputePipelineState(&loop_pso);
        // SAFETY: out_buf is a live 4096-byte buffer; busy_kernel writes
        // index gid % 1024, the light kernel 16 floats.
        unsafe { encoder.setBuffer_offset_atIndex(Some(&out_buf), 0, 0) };
        let threads = if heavy { 32768 } else { 16 };
        encoder.dispatchThreads_threadsPerThreadgroup(size(threads), size(16));
        encoder.endEncoding();
        cb.commit();
        if no_wait {
            last_cb = Some(cb);
        } else {
            cb.waitUntilCompleted();
        }
        if sleep_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(sleep_ms));
        }
    }
    if let Some(cb) = last_cb {
        cb.waitUntilCompleted();
    }

    // Let the hook flush completion handlers before exit.
    std::thread::sleep(std::time::Duration::from_millis(750));
    println!("harness done");
}

/// A 1-D grid / threadgroup size.
#[cfg(target_os = "macos")]
fn size(width: usize) -> objc2_metal::MTLSize {
    objc2_metal::MTLSize {
        width,
        height: 1,
        depth: 1,
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("smeltr-metal-harness only runs on macOS");
    std::process::exit(1);
}

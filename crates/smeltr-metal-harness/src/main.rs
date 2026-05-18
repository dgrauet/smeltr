#[cfg(target_os = "macos")]
fn main() {
    use metal::*;
    let device = Device::system_default().expect("no Metal device");
    let queue = device.new_command_queue();

    // Buffer alloc — should produce a BufferAlloc event.
    let buf = device.new_buffer(4096, MTLResourceOptions::StorageModeShared);
    buf.set_label("smeltr-harness-buffer");

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

    // Let the hook flush completion handlers before exit.
    std::thread::sleep(std::time::Duration::from_millis(750));
    println!("harness done");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("smeltr-metal-harness only runs on macOS");
    std::process::exit(1);
}

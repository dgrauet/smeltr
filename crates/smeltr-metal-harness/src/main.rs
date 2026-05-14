#[cfg(target_os = "macos")]
fn main() {
    use metal::*;
    let device = Device::system_default().expect("no Metal device");
    let queue = device.new_command_queue();

    // Buffer alloc — should produce a BufferAlloc event.
    let buf = device.new_buffer(4096, MTLResourceOptions::StorageModeShared);
    buf.set_label("smeltr-harness-buffer");

    // No-op command buffer with a blit encoder.
    let cb = queue.new_command_buffer();
    cb.set_label("smeltr-harness-noop");
    let encoder = cb.new_blit_command_encoder();
    encoder.end_encoding();
    cb.commit();
    cb.wait_until_completed();

    // Let the hook flush completion handlers before exit.
    std::thread::sleep(std::time::Duration::from_millis(200));
    println!("harness done");
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("smeltr-metal-harness only runs on macOS");
    std::process::exit(1);
}

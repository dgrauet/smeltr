//! Verifies the build.rs script staged a real Mach-O dylib in OUT_DIR
//! that ends up included via include_bytes! in embedded_dylib.rs.

use smeltr_cli::embedded_dylib::EMBEDDED_DYLIB;

#[test]
fn embedded_dylib_is_a_macho_dylib() {
    assert!(
        EMBEDDED_DYLIB.len() > 1024,
        "embedded dylib suspiciously small: {} bytes",
        EMBEDDED_DYLIB.len()
    );
    // Mach-O 64-bit magic (little-endian): 0xFEEDFACF
    let magic = u32::from_le_bytes([
        EMBEDDED_DYLIB[0],
        EMBEDDED_DYLIB[1],
        EMBEDDED_DYLIB[2],
        EMBEDDED_DYLIB[3],
    ]);
    assert_eq!(
        magic, 0xFEEDFACF,
        "not a Mach-O 64-bit binary; first 4 bytes = {:08x}",
        magic
    );
}

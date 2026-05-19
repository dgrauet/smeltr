use smeltr_metal_ring::wire::*;

const HEADER: &str = include_str!("../include/smeltr_ring.h");

fn header_contains(needle: &str) -> bool {
    HEADER.contains(needle)
}

#[test]
fn magic_matches() {
    assert!(
        header_contains(&format!("0x{:08X}u", RING_MAGIC)),
        "header must define MAGIC as 0x{:08X}u",
        RING_MAGIC
    );
}

#[test]
fn version_matches() {
    assert!(header_contains(&format!(
        "SMELTR_RING_VERSION {}u",
        RING_VERSION
    )));
}

#[test]
fn kinds_match() {
    let pairs = [
        ("SMELTR_KIND_PAD", kind::PAD),
        ("SMELTR_KIND_CB_COMMITTED", kind::CB_COMMITTED),
        ("SMELTR_KIND_CB_SCHEDULED", kind::CB_SCHEDULED),
        ("SMELTR_KIND_CB_COMPLETED", kind::CB_COMPLETED),
        ("SMELTR_KIND_CB_WARNING", kind::CB_WARNING),
        ("SMELTR_KIND_HEAP_ALLOC", kind::HEAP_ALLOC),
        ("SMELTR_KIND_HEAP_FREE", kind::HEAP_FREE),
        ("SMELTR_KIND_BUFFER_ALLOC", kind::BUFFER_ALLOC),
        ("SMELTR_KIND_BUFFER_FREE", kind::BUFFER_FREE),
        ("SMELTR_KIND_TEXTURE_ALLOC", kind::TEXTURE_ALLOC),
        ("SMELTR_KIND_TEXTURE_FREE", kind::TEXTURE_FREE),
        ("SMELTR_KIND_DROPPED", kind::DROPPED),
        ("SMELTR_KIND_SKIPPED", kind::SKIPPED),
        ("SMELTR_KIND_CB_OPS", kind::CB_OPS),
        ("SMELTR_KIND_DEVICE_MEM_SAMPLE", kind::DEVICE_MEM_SAMPLE),
    ];
    // Normalize whitespace runs in the header so column-aligned #defines match.
    let normalized: String = HEADER.split_whitespace().collect::<Vec<_>>().join(" ");
    for (name, val) in pairs {
        let needle = format!("#define {} {}u", name, val);
        assert!(
            normalized.contains(&needle),
            "header must define `{}` to {}",
            name,
            val
        );
    }
}

#[test]
fn cb_ops_symbol_len_none_sentinel_matches() {
    // Both sides must agree on the "no symbol" sentinel for CB_OPS frames.
    assert_eq!(CB_OPS_SYMBOL_LEN_NONE, 0xFFFF_FFFF);
    let normalized: String = HEADER.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        normalized.contains("#define SMELTR_CB_OPS_SYMBOL_LEN_NONE 0xFFFFFFFFu"),
        "header must define SMELTR_CB_OPS_SYMBOL_LEN_NONE to 0xFFFFFFFFu"
    );
}

#[test]
fn header_size_const_correct() {
    assert_eq!(RING_HEADER_BYTES, 40);
    assert_eq!(FRAME_HEADER_BYTES, 16);
}

#[test]
fn smeltr_ring_header_struct_present_in_header() {
    assert!(HEADER.contains("smeltr_ring_header_t"));
    assert!(HEADER.contains("smeltr_frame_header_t"));
}

//! A recording's rings: the main one plus one `<ring>.<pid>` per extra
//! hooked process (#264).

use smeltr_metal_ring::{create_ring, remove_ring_family, ring_family};

#[test]
fn family_is_the_main_ring_then_its_pid_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("abc.ring");
    drop(create_ring(&main, 1 << 12).unwrap());
    for name in [
        "abc.ring.4242",
        "abc.ring.17",
        "abc.ring.99.tmp", // still being created by the hook
        "abc.ring.x1",     // not a pid
        "abcd.ring.5",     // another recording
        "abc.ring.",       // empty suffix
    ] {
        std::fs::write(dir.path().join(name), b"").unwrap();
    }
    let got: Vec<String> = ring_family(&main)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(got, ["abc.ring", "abc.ring.17", "abc.ring.4242"]);
}

#[test]
fn family_of_a_missing_ring_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    assert!(ring_family(&dir.path().join("gone.ring")).is_empty());
}

#[test]
fn removing_the_family_leaves_other_files() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("abc.ring");
    drop(create_ring(&main, 1 << 12).unwrap());
    for name in ["abc.ring.4242", "abc.ring.99.tmp", "abcd.ring"] {
        std::fs::write(dir.path().join(name), b"").unwrap();
    }
    remove_ring_family(&main);
    let mut left: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    // A half-created sibling goes too: nothing will ever drain it.
    assert_eq!(left, ["abcd.ring"]);
}

//! ENOSPC on a real, tiny HFS+ volume (#268).
//!
//! These tests create a 4 MB disk image with `hdiutil`, point `SMELTR_HOME` at
//! it, fill the remaining space with a filler file, keep writing events while
//! the disk is full, then free the space and finish the session. Every event
//! the writer accepted must be readable afterwards.
//!
//! Soft-skipped (with a message) when `hdiutil` is unavailable or refuses to
//! attach, e.g. in a sandbox without disk-image support.

use smeltr_core::chunked::ChunkConfig;
use smeltr_core::event::{Event, Payload, Source};
use smeltr_core::session::{SessionId, SessionMetadata};
use smeltr_core::writer::SessionWriter;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A mounted 4 MB HFS+ image, detached on drop.
struct TinyVolume {
    _tmp: tempfile::TempDir,
    mount: PathBuf,
}

impl TinyVolume {
    fn new() -> Option<Self> {
        let tmp = tempfile::tempdir().ok()?;
        let dmg = tmp.path().join("v.dmg");
        let mount = tmp.path().join("mnt");
        std::fs::create_dir_all(&mount).ok()?;
        let created = Command::new("hdiutil")
            .args(["create", "-quiet", "-size", "4m", "-fs", "HFS+"])
            .args(["-volname", "smeltrtest"])
            .arg(&dmg)
            .status();
        if !matches!(created, Ok(s) if s.success()) {
            eprintln!("SKIP: hdiutil create unavailable ({created:?})");
            return None;
        }
        let attached = Command::new("hdiutil")
            .args(["attach", "-quiet", "-nobrowse", "-mountpoint"])
            .arg(&mount)
            .arg(&dmg)
            .status();
        if !matches!(attached, Ok(s) if s.success()) {
            eprintln!("SKIP: hdiutil attach failed ({attached:?})");
            return None;
        }
        Some(Self { _tmp: tmp, mount })
    }

    fn path(&self) -> &Path {
        &self.mount
    }

    /// Fill every free byte with a filler file; returns its path.
    fn fill(&self) -> PathBuf {
        let p = self.mount.join("filler");
        let mut f = std::fs::File::create(&p).unwrap();
        let big = vec![0xA5u8; 64 * 1024];
        while f.write_all(&big).is_ok() {}
        let small = [0xA5u8; 512];
        while f.write_all(&small).is_ok() {}
        let one = [0xA5u8; 1];
        while f.write_all(&one).is_ok() {}
        let _ = f.flush();
        p
    }
}

impl Drop for TinyVolume {
    fn drop(&mut self) {
        let ok = Command::new("hdiutil")
            .args(["detach", "-quiet"])
            .arg(&self.mount)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            let _ = Command::new("hdiutil")
                .args(["detach", "-quiet", "-force"])
                .arg(&self.mount)
                .status();
        }
    }
}

fn ev(i: u64) -> Event {
    Event {
        ts_mono_ns: i,
        ts_wall_ns: i,
        session_id: uuid::Uuid::nil(),
        source: Source::Mark,
        pid: None,
        seq: i,
        payload: Payload::Mark {
            // Mostly incompressible so a few hundred events take real space.
            label: format!(
                "mark-{i}-{}",
                (0..12u64)
                    .map(|k| format!("{:016x}", (i * 31 + k).wrapping_mul(0x9E37_79B9_7F4A_7C15)))
                    .collect::<String>()
            ),
            fields: Default::default(),
        },
    }
}

/// Write `n` events starting at `*next`, flushing every `flush_every` like the
/// daemon's periodic flush. Returns the seqs the writer accepted.
///
/// A high event rate (few flushes per zstd block) matters: that is when the
/// encoder drains its output *inside* `write_event`, so an ENOSPC can land
/// between an event's length prefix and its body.
fn write_batch(w: &mut SessionWriter, next: &mut u64, n: u64, flush_every: u64) -> Vec<u64> {
    let mut acked = Vec::new();
    for _ in 0..n {
        let i = *next;
        *next += 1;
        if w.write_event(&ev(i)).is_ok() {
            acked.push(i);
        }
        if i % flush_every == flush_every - 1 {
            let _ = w.flush();
        }
    }
    acked
}

fn run_enospc_then_free(chunked: bool) {
    let Some(vol) = TinyVolume::new() else { return };
    std::env::set_var("SMELTR_HOME", vol.path());
    std::env::remove_var("SMELTR_SESSION_INDEX");
    let meta = SessionMetadata::now_starting(SessionId::new());
    let cfg = chunked.then(ChunkConfig::default);
    let mut w = SessionWriter::create_with_chunk_config(meta, cfg).unwrap();
    let dir = w.dir().to_path_buf();
    let mut next = 0u64;

    let mut acked = write_batch(&mut w, &mut next, 300, 50);
    w.flush().unwrap();

    let filler = vol.fill();
    acked.extend(write_batch(&mut w, &mut next, 3000, 1000));
    assert!(w.flush().is_err(), "the disk is full: flush must report it");

    std::fs::remove_file(&filler).unwrap();
    acked.extend(write_batch(&mut w, &mut next, 300, 50));
    w.flush().unwrap();
    w.finalize(Some(0), "2026-09-27T00:00:00Z".into()).unwrap();

    let got: Vec<u64> = smeltr_core::reader::read_events(&dir)
        .unwrap()
        .into_iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(
        got.len(),
        acked.len(),
        "every accepted event must be readable after the disk frees up"
    );
    assert_eq!(got, acked, "same events, same order");
}

/// `write_metadata` on a full disk must leave the previous metadata intact:
/// a plain `fs::write` truncates first, and the session then had no
/// readable metadata at all.
#[test]
#[serial_test::serial]
fn metadata_survives_a_failed_rewrite_on_a_full_disk() {
    let Some(vol) = TinyVolume::new() else { return };
    std::env::set_var("SMELTR_HOME", vol.path());
    let meta = SessionMetadata::now_starting(SessionId::new());
    let w = SessionWriter::create(meta.clone()).unwrap();
    let dir = w.dir().to_path_buf();
    drop(w);
    let _filler = vol.fill();
    let mut bigger = meta.clone();
    bigger.argv = vec!["x".repeat(64 * 1024)];
    assert!(smeltr_core::session::write_metadata(&dir, &bigger).is_err());
    let read = smeltr_core::reader::read_metadata(&dir).expect("old metadata still readable");
    assert_eq!(read.session_id, meta.session_id);
}

#[test]
#[serial_test::serial]
fn legacy_writer_survives_enospc_then_free() {
    run_enospc_then_free(false);
}

#[test]
#[serial_test::serial]
fn chunked_writer_survives_enospc_then_free() {
    run_enospc_then_free(true);
}

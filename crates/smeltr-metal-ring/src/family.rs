//! A recording's rings. `smeltr record` creates one ring; every descendant
//! of the recorded command inherits the hook and its path, and each hooked
//! process after the first writes its own `<ring>.<pid>` (#264). The hook
//! creates a sibling as `<ring>.<pid>.tmp` and renames it once its header
//! is written, so a reader never sees a half-initialised one.

use std::path::{Path, PathBuf};

/// The main ring (if it exists) followed by its `<ring>.<pid>` siblings,
/// in ascending pid order.
pub fn ring_family(main: &Path) -> Vec<PathBuf> {
    let mut family = Vec::new();
    if main.exists() {
        family.push(main.to_path_buf());
    }
    let mut siblings: Vec<(u32, PathBuf)> = siblings_matching(main, |suffix| suffix.parse().ok());
    siblings.sort_by_key(|(pid, _)| *pid);
    family.extend(siblings.into_iter().map(|(_, p)| p));
    family
}

/// Remove the main ring, its siblings and any sibling still being created.
/// Best effort: a ring that is already gone is not an error.
pub fn remove_ring_family(main: &Path) {
    let _ = std::fs::remove_file(main);
    let doomed = siblings_matching(main, |suffix| {
        let pid = suffix.strip_suffix(".tmp").unwrap_or(suffix);
        pid.parse::<u32>().ok()
    });
    for (_, path) in doomed {
        let _ = std::fs::remove_file(path);
    }
}

/// Files next to `main` named `<main name>.<suffix>` where `parse(suffix)`
/// accepts the suffix.
fn siblings_matching(main: &Path, parse: impl Fn(&str) -> Option<u32>) -> Vec<(u32, PathBuf)> {
    let (Some(dir), Some(name)) = (main.parent(), main.file_name().and_then(|n| n.to_str())) else {
        return Vec::new();
    };
    let prefix = format!("{name}.");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let file = e.file_name();
            let suffix = file.to_str()?.strip_prefix(&prefix)?;
            let pid = parse(suffix)?;
            Some((pid, e.path()))
        })
        .collect()
}

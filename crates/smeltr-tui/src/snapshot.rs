//! Snapshot the live `UiState` to a JSON file (the `s` keybind).

use crate::state::UiState;
use smeltr_core::session::sessions_root;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn snapshot_json(state: &UiState) -> serde_json::Result<String> {
    serde_json::to_string_pretty(state)
}

pub fn snapshots_dir() -> PathBuf {
    sessions_root()
        .parent()
        .map(|p| p.join("snapshots"))
        .unwrap_or_else(|| PathBuf::from("snapshots"))
}

pub fn write_snapshot(state: &UiState) -> std::io::Result<PathBuf> {
    let dir = snapshots_dir();
    std::fs::create_dir_all(&dir)?;
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("snapshot-{ns}.json"));
    let json = snapshot_json(state).map_err(std::io::Error::other)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_json_is_valid_and_has_fields() {
        let st = UiState {
            events_total: 42,
            ..Default::default()
        };
        let s = snapshot_json(&st).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["events_total"], 42);
    }

    #[test]
    #[serial_test::serial]
    fn write_snapshot_creates_json_file_under_home() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let path = write_snapshot(&UiState::default()).unwrap();
        assert!(path.exists());
        assert!(
            path.starts_with(home.path()),
            "path {path:?} not under home"
        );
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&content).is_ok());
        std::env::remove_var("SMELTR_HOME");
    }
}

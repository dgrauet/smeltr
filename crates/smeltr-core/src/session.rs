//! Session identity, metadata, and on-disk layout helpers.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
    pub fn short(&self) -> String {
        self.0.as_simple().to_string()[..8].to_string()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.as_simple())
    }
}

impl std::str::FromStr for SessionId {
    type Err = uuid::Error;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub session_id: SessionId,
    pub started_rfc3339: String,
    pub ended_rfc3339: Option<String>,
    pub host: String,
    pub mlx_version: Option<String>,
    pub exit_code: Option<i32>,
    pub argv: Vec<String>,
}

impl SessionMetadata {
    pub fn now_starting(session_id: SessionId) -> Self {
        Self {
            session_id,
            started_rfc3339: OffsetDateTime::now_utc().format(&Rfc3339).unwrap(),
            ended_rfc3339: None,
            host: hostname_or_unknown(),
            mlx_version: None,
            exit_code: None,
            argv: std::env::args().collect(),
        }
    }
}

fn hostname_or_unknown() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// Returns `$SMELTR_HOME/sessions` (defaulting to `$HOME/.smeltr/sessions`).
pub fn sessions_root() -> PathBuf {
    if let Ok(p) = std::env::var("SMELTR_HOME") {
        PathBuf::from(p).join("sessions")
    } else {
        dirs_home().join(".smeltr").join("sessions")
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME must be set")
}

/// Directory name for a session: `YYYY-MM-DD-HHMMSS-<8 hex>`.
pub fn session_dir_name(meta: &SessionMetadata) -> String {
    let t = OffsetDateTime::parse(&meta.started_rfc3339, &Rfc3339)
        .expect("metadata wrote a valid RFC3339 timestamp");
    format!(
        "{:04}-{:02}-{:02}-{:02}{:02}{:02}-{}",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        meta.session_id.short(),
    )
}

pub fn session_dir(meta: &SessionMetadata) -> PathBuf {
    sessions_root().join(session_dir_name(meta))
}

pub fn metadata_path(dir: &Path) -> PathBuf {
    dir.join("metadata.toml")
}
pub fn events_path(dir: &Path) -> PathBuf {
    dir.join("events.cbor")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_round_trips_through_string() {
        let id = SessionId::new();
        let s = id.to_string();
        let back: SessionId = s.parse().unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn short_is_eight_chars() {
        assert_eq!(SessionId::new().short().len(), 8);
    }

    #[test]
    fn dir_name_starts_with_year() {
        let m = SessionMetadata::now_starting(SessionId::new());
        let name = session_dir_name(&m);
        assert!(name.starts_with("20"), "got {name}");
        assert!(name.ends_with(&m.session_id.short()), "got {name}");
    }
}

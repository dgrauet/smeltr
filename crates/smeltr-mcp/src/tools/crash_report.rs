//! `get_crash_report` tool: read the first .ips file from a session.

use crate::types::{resolve_session, ToolError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Params {
    pub session: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub crash_report_path: Option<String>,
    pub text: Option<String>,
    pub size_bytes: Option<u64>,
}

pub fn run(params: Params) -> Result<Response, ToolError> {
    let dir = resolve_session(&params.session)?;
    let crash_dir = dir.join("crash-reports");
    if !crash_dir.exists() {
        return Ok(Response {
            crash_report_path: None,
            text: None,
            size_bytes: None,
        });
    }
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&crash_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("ips"))
        .collect();
    entries.sort();
    let Some(first) = entries.first() else {
        return Ok(Response {
            crash_report_path: None,
            text: None,
            size_bytes: None,
        });
    };
    let text = std::fs::read_to_string(first)?;
    let size_bytes = std::fs::metadata(first)?.len();
    Ok(Response {
        crash_report_path: Some(first.display().to_string()),
        text: Some(text),
        size_bytes: Some(size_bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::session::{SessionId, SessionMetadata};
    use smeltr_core::writer::SessionWriter;

    #[test]
    #[serial_test::serial]
    fn returns_none_when_no_crash_dir() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let w = SessionWriter::create(meta).unwrap();
        let _dir = w.dir().to_path_buf();
        drop(w);
        let resp = run(Params {
            session: id.short(),
        })
        .unwrap();
        assert!(resp.text.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn returns_first_ips_text() {
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("SMELTR_HOME", home.path());
        let id = SessionId::new();
        let meta = SessionMetadata::now_starting(id);
        let w = SessionWriter::create(meta).unwrap();
        let dir = w.dir().to_path_buf();
        drop(w);
        let crash_dir = dir.join("crash-reports");
        std::fs::create_dir_all(&crash_dir).unwrap();
        std::fs::write(crash_dir.join("python-2026-05-14.ips"), "fake ips content").unwrap();

        let resp = run(Params {
            session: id.short(),
        })
        .unwrap();
        assert_eq!(resp.text.as_deref(), Some("fake ips content"));
        assert_eq!(resp.size_bytes, Some(16));
    }
}

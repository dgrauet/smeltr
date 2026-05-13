use serde::Deserialize;
use smeltr_core::event::Payload;

#[derive(Deserialize)]
struct LogShowLine {
    #[serde(default)]
    subsystem: String,
    #[serde(default)]
    category: String,
    #[serde(default, rename = "eventMessage")]
    message: String,
}

pub fn parse_line(s: &str) -> Option<Payload> {
    let raw: LogShowLine = serde_json::from_str(s).ok()?;
    if raw.message.is_empty() && raw.subsystem.is_empty() {
        return None;
    }
    Some(Payload::OsLogLine {
        ts_wall_ns: 0,
        subsystem: raw.subsystem,
        category: raw.category,
        message: raw.message,
    })
}

pub const SUBSYSTEM_FILTERS: &[&str] = &[
    "com.apple.gpurestart",
    "com.apple.GPUWrangler",
    "com.apple.coreanalytics",
];

pub fn predicate() -> String {
    let parts: Vec<String> = SUBSYSTEM_FILTERS
        .iter()
        .map(|s| format!(r#"subsystem == "{s}""#))
        .collect();
    format!(
        "({}) OR (eventMessage CONTAINS \"GPU watchdog\")",
        parts.join(" OR ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_log_line() {
        let line =
            r#"{"subsystem":"com.apple.gpurestart","category":"x","eventMessage":"GPU restarted"}"#;
        let p = parse_line(line).unwrap();
        assert!(matches!(
            p,
            Payload::OsLogLine { ref subsystem, ref message, .. }
            if subsystem == "com.apple.gpurestart" && message == "GPU restarted"
        ));
    }

    #[test]
    fn skips_empty_lines() {
        assert!(parse_line("{}").is_none());
        assert!(parse_line("").is_none());
    }

    #[test]
    fn predicate_includes_all_subsystems() {
        let p = predicate();
        for s in SUBSYSTEM_FILTERS {
            assert!(p.contains(s));
        }
        assert!(p.contains("GPU watchdog"));
    }
}

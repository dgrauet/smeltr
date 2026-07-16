use serde::Deserialize;
use smeltr_core::event::Payload;

#[derive(Deserialize)]
struct Header {
    #[allow(dead_code)]
    #[serde(default)]
    app_name: String,
}

#[derive(Deserialize)]
struct Body {
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    exception: Option<Exception>,
    #[serde(default)]
    termination: Option<Termination>,
}

#[derive(Deserialize)]
struct Exception {
    #[serde(default, rename = "type")]
    ty: Option<String>,
    #[serde(default)]
    codes: Option<String>,
    #[serde(default)]
    signal: Option<String>,
    #[serde(default)]
    subtype: Option<String>,
}

#[derive(Deserialize)]
struct Termination {
    #[serde(default)]
    signal: Option<String>,
}

pub fn parse_ips(content: &str, path: &str) -> Option<Payload> {
    // Line 1 is the single-line header JSON; the body JSON is everything
    // after it — single-line on older macOS, pretty-printed across
    // thousands of lines on macOS 15/26 (#151).
    let (header_line, body_text) = content.split_once('\n')?;
    let _hdr: Header = serde_json::from_str(header_line).ok()?;
    let body: Body = serde_json::from_str(body_text).ok()?;

    let mut codes_out = Vec::new();
    let mut summary = String::new();
    let signal = body
        .termination
        .as_ref()
        .and_then(|t| t.signal.clone())
        .or_else(|| body.exception.as_ref().and_then(|e| e.signal.clone()));

    if let Some(exc) = &body.exception {
        if let Some(t) = &exc.ty {
            summary.push_str(t);
        }
        if let Some(s) = &exc.subtype {
            for tok in s.split_whitespace() {
                if tok.starts_with("kIOGPU") || tok.starts_with("(0x") {
                    codes_out.push(tok.trim_matches(|c: char| c == '(' || c == ')').to_string());
                }
            }
            if !summary.is_empty() {
                summary.push_str(": ");
            }
            summary.push_str(s);
        }
        if let Some(c) = &exc.codes {
            codes_out.push(c.clone());
        }
    }

    Some(Payload::CrashReportEmitted {
        path: path.into(),
        crashed_pid: body.pid,
        signal,
        exception_codes: codes_out,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/sample.ips");

    #[test]
    fn parses_fixture_and_finds_gpu_code() {
        let p = parse_ips(FIXTURE, "/x/sample.ips").expect("parse failed");
        let Payload::CrashReportEmitted {
            crashed_pid,
            signal,
            exception_codes,
            summary,
            path,
        } = p
        else {
            panic!()
        };
        assert_eq!(path, "/x/sample.ips");
        assert_eq!(crashed_pid, Some(38291));
        assert_eq!(signal.as_deref(), Some("SIGABRT"));
        assert!(
            exception_codes.iter().any(|c| c.contains("kIOGPU")),
            "codes: {exception_codes:?}"
        );
        assert!(summary.contains("kIOGPU"), "summary: {summary}");
    }

    #[test]
    fn empty_input_returns_none() {
        assert!(parse_ips("", "/x").is_none());
    }

    #[test]
    fn parses_pretty_printed_multiline_body() {
        // Real ReportCrash output on macOS 15/26: single-line header, then
        // the body JSON pretty-printed across thousands of lines (#151).
        let fixture = include_str!("../tests/fixtures/sample_multiline.ips");
        let p = parse_ips(fixture, "/x/multi.ips").expect("parse failed");
        let Payload::CrashReportEmitted {
            crashed_pid,
            signal,
            summary,
            ..
        } = p
        else {
            panic!()
        };
        assert_eq!(crashed_pid, Some(11672));
        assert_eq!(signal.as_deref(), Some("SIGABRT"));
        assert!(summary.contains("EXC_CRASH"), "summary: {summary}");
    }

    #[test]
    fn truncated_body_returns_none() {
        // Partial read while ReportCrash is still writing.
        let fixture = include_str!("../tests/fixtures/sample_multiline.ips");
        assert!(parse_ips(&fixture[..fixture.len() / 2], "/x").is_none());
    }

    #[test]
    fn garbage_returns_none() {
        assert!(parse_ips("not json\nstill not", "/x").is_none());
    }
}

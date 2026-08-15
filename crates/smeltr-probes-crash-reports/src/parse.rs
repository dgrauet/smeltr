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

#[derive(Deserialize)]
struct JetsamHeader {
    #[serde(default)]
    bug_type: Option<String>,
}

#[derive(Deserialize)]
struct JetsamBody {
    #[serde(default, rename = "memoryStatus")]
    memory_status: Option<JetsamMemoryStatus>,
    #[serde(default)]
    processes: Vec<JetsamProcess>,
    #[serde(default, rename = "largestProcess")]
    largest_process: Option<String>,
}

#[derive(Deserialize)]
struct JetsamMemoryStatus {
    #[serde(default, rename = "pageSize")]
    page_size: Option<u64>,
}

#[derive(Deserialize)]
struct JetsamProcess {
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    rpages: Option<u64>,
    #[serde(default, rename = "lifetimeMax")]
    lifetime_max: Option<u64>,
}

/// Extrait le processus tué d'un rapport jetsam. On retient l'entrée nommée
/// par `largestProcess` quand elle existe, sinon celle de plus grande
/// empreinte : c'est celle que le noyau a sacrifiée.
fn parse_jetsam(body_text: &str, path: &str) -> Option<Payload> {
    let body: JetsamBody = serde_json::from_str(body_text).ok()?;
    let page_size = body
        .memory_status
        .as_ref()
        .and_then(|m| m.page_size)
        .unwrap_or(16_384);

    let victim = body
        .largest_process
        .as_ref()
        .and_then(|want| {
            body.processes
                .iter()
                .find(|p| p.name.as_deref() == Some(want.as_str()))
        })
        .or_else(|| body.processes.iter().max_by_key(|p| p.rpages.unwrap_or(0)))?;

    Some(Payload::JetsamKill {
        path: path.into(),
        killed_pid: victim.pid,
        killed_name: victim.name.clone().unwrap_or_default(),
        footprint_bytes: victim.rpages.unwrap_or(0).saturating_mul(page_size),
        lifetime_max_bytes: victim.lifetime_max.unwrap_or(0).saturating_mul(page_size),
        page_size,
    })
}

pub fn parse_ips(content: &str, path: &str) -> Option<Payload> {
    // Line 1 is the single-line header JSON; the body JSON is everything
    // after it — single-line on older macOS, pretty-printed across
    // thousands of lines on macOS 15/26 (#151).
    let (header_line, body_text) = content.split_once('\n')?;

    // Un rapport jetsam n'a ni `exception` ni `termination` : sans ce
    // branchement il se désérialise en un CrashReportEmitted entièrement
    // vide, ce qui est pire qu'un rejet.
    if let Ok(jh) = serde_json::from_str::<JetsamHeader>(header_line) {
        if jh.bug_type.as_deref() == Some("298") {
            return parse_jetsam(body_text, path);
        }
    }

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

    const JETSAM: &str = include_str!("../tests/fixtures/jetsam.ips");

    #[test]
    fn jetsam_report_yields_kill_with_footprint() {
        let p = parse_ips(JETSAM, "/x/jetsam.ips").expect("parse failed");
        let Payload::JetsamKill {
            killed_pid,
            killed_name,
            footprint_bytes,
            lifetime_max_bytes,
            page_size,
            ..
        } = p
        else {
            panic!("attendu JetsamKill, eu {p:?}");
        };
        assert_eq!(killed_pid, Some(4242));
        assert_eq!(killed_name, "python");
        assert_eq!(page_size, 16384);
        // 1 310 720 pages × 16 Ko = 21,47 Go — la signature du bug ltx-2-mlx #74.
        assert_eq!(footprint_bytes, 1_310_720 * 16_384);
        assert_eq!(lifetime_max_bytes, 1_400_000 * 16_384);
    }

    /// Régression : avant ce correctif, un rapport jetsam se désérialisait
    /// silencieusement en un CrashReportEmitted entièrement vide, ce qui est
    /// pire qu'un rejet — la session disait qu'un rapport existait sans rien
    /// en dire. Ce test épingle que ça ne peut plus arriver.
    #[test]
    fn jetsam_report_is_never_an_empty_crash_report() {
        let p = parse_ips(JETSAM, "/x/jetsam.ips").expect("parse failed");
        assert!(
            !matches!(
                p,
                Payload::CrashReportEmitted {
                    crashed_pid: None,
                    signal: None,
                    ..
                }
            ),
            "coquille vide de retour : {p:?}"
        );
    }

    /// Le chemin nominal ne bouge pas.
    #[test]
    fn regular_crash_report_still_parses_as_before() {
        let p = parse_ips(FIXTURE, "/x/sample.ips").expect("parse failed");
        assert!(matches!(p, Payload::CrashReportEmitted { .. }));
    }
}

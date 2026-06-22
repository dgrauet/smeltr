//! `smeltr breakdown` command.

use anyhow::{anyhow, Context, Result};
use smeltr_analyzer::{
    compute_breakdown, render_chrome_trace, render_ops_flat, render_table, ModuleBreakdown,
    OpGroupBy,
};
use smeltr_core::event::FieldValue;
use smeltr_core::reader::read_events;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Parse a `key=value` flag into a (String, FieldValue) tuple.
///
/// Value type inference (matches `_coerce_fields` semantics on the
/// Python side):
///   - "true" / "false" → Bool
///   - parses as i64 → Int
///   - parses as f64 (containing `.` or `e`) → Float
///   - otherwise → String
fn parse_field_kv(s: &str) -> Result<(String, FieldValue), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got {s:?}"))?;
    let key = k.trim().to_string();
    if key.is_empty() {
        return Err(format!("empty key in {s:?}"));
    }
    let v = v.trim();
    let val = if v.eq_ignore_ascii_case("true") {
        FieldValue::Bool(true)
    } else if v.eq_ignore_ascii_case("false") {
        FieldValue::Bool(false)
    } else if let Ok(i) = v.parse::<i64>() {
        FieldValue::Int(i)
    } else if let Ok(f) = v.parse::<f64>() {
        FieldValue::Float(f)
    } else {
        FieldValue::String(v.to_string())
    };
    Ok((key, val))
}

#[allow(clippy::too_many_arguments)]
pub fn run(
    id: Option<String>,
    last: bool,
    include_ambient: bool,
    top: usize,
    depth: u16,
    flamegraph: Option<PathBuf>,
    chrome_trace: Option<PathBuf>,
    top_ops: usize,
    no_ops: bool,
    ops_flat: bool,
    field_filter_raw: Vec<String>,
) -> Result<()> {
    let dir = crate::session_resolver::resolve(id, last, include_ambient)?;
    let events =
        read_events(&dir).with_context(|| format!("reading events from {}", dir.display()))?;
    if events.is_empty() {
        println!("no events captured - was `smeltr record` used and the model exercised?");
        return Ok(());
    }

    // Parse --field key=value flags.
    let field_filter: BTreeMap<String, FieldValue> = field_filter_raw
        .iter()
        .map(|s| parse_field_kv(s).map_err(|e| anyhow!("{e}")))
        .collect::<Result<_>>()?;

    let mut root = compute_breakdown(events).context("computing breakdown")?;

    // Apply field filter before rendering.
    if !field_filter.is_empty() {
        prune_by_field_filter(&mut root, &field_filter);
    }

    if ops_flat {
        println!("{}", render_ops_flat(&root, OpGroupBy::Name, top));
    } else {
        let show_ops = !no_ops;
        println!("{}", render_table(&root, top, depth, top_ops, show_ops));
    }

    if let Some(path) = flamegraph {
        write_flamegraph(&path, &root)
            .with_context(|| format!("writing flamegraph to {}", path.display()))?;
        println!("flamegraph written to {}", path.display());
    }
    if let Some(path) = chrome_trace {
        let json = render_chrome_trace(&root);
        std::fs::write(&path, json)
            .with_context(|| format!("writing chrome trace to {}", path.display()))?;
        println!("chrome trace written to {}", path.display());
    }
    Ok(())
}

/// Recursively prunes nodes that don't match `filter` and have no matching
/// descendants. A node matches when its `fields` map is a superset of `filter`.
fn prune_by_field_filter(
    node: &mut ModuleBreakdown,
    filter: &BTreeMap<String, FieldValue>,
) -> bool {
    let mut any_child_matches = false;
    let mut kept = Vec::with_capacity(node.children.len());
    for mut child in std::mem::take(&mut node.children) {
        if prune_by_field_filter(&mut child, filter) {
            any_child_matches = true;
            kept.push(child);
        }
    }
    node.children = kept;

    let self_matches = filter.iter().all(|(k, v)| node.fields.get(k) == Some(v));
    self_matches || any_child_matches
}

fn write_flamegraph(path: &Path, root: &ModuleBreakdown) -> Result<()> {
    let mut lines: Vec<String> = Vec::new();
    fn walk(n: &ModuleBreakdown, prefix: &str, out: &mut Vec<String>) {
        let here = if prefix.is_empty() {
            n.qualname.clone()
        } else {
            format!("{prefix};{}", n.qualname)
        };
        if n.gpu_ns_self > 0 {
            out.push(format!("{here} {}", n.gpu_ns_self));
        }
        for c in &n.children {
            walk(c, &here, out);
        }
    }
    for c in &root.children {
        walk(c, "", &mut lines);
    }
    let folded = lines.join("\n");
    let mut opts = inferno::flamegraph::Options::default();
    opts.title = "smeltr inference breakdown (ns GPU self)".into();
    opts.count_name = "ns".into();
    let mut svg = std::fs::File::create(path)?;
    inferno::flamegraph::from_reader(&mut opts, folded.as_bytes(), &mut svg)
        .map_err(|e| anyhow!("inferno: {e}"))?;
    svg.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_field_kv_infers_types() {
        assert_eq!(
            parse_field_kv("step=5").unwrap(),
            ("step".to_string(), FieldValue::Int(5))
        );
        assert_eq!(
            parse_field_kv("sigma=0.5").unwrap(),
            ("sigma".to_string(), FieldValue::Float(0.5))
        );
        assert_eq!(
            parse_field_kv("flag=true").unwrap(),
            ("flag".to_string(), FieldValue::Bool(true))
        );
        assert_eq!(
            parse_field_kv("name=ltx2").unwrap(),
            ("name".to_string(), FieldValue::String("ltx2".into()))
        );
    }

    #[test]
    fn parse_field_kv_rejects_malformed() {
        assert!(parse_field_kv("no-equals-sign").is_err());
        assert!(parse_field_kv("=novalue").is_err());
    }
}

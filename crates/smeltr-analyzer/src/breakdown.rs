//! Module-level GPU breakdown computed from a session's events.

use serde::{Deserialize, Serialize};
use smeltr_core::event::{Event, Payload};
use std::collections::HashMap;

/// Reserved qualname for time not attributable to any module call.
pub const UNSCOPED: &str = "<unscoped>";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OpAttribution {
    pub name: String,
    pub gpu_ns: u64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModuleBreakdown {
    pub qualname: String,
    pub class_name: String,
    pub calls: u64,
    pub gpu_ns_self: u64,
    pub gpu_ns_subtree: u64,
    pub eval_count: u64,
    pub cb_count: u64,
    pub children: Vec<ModuleBreakdown>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<OpAttribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostics: Option<Diagnostics>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Diagnostics {
    pub unscoped_gpu_ns: u64,
    pub unmatched_cb_count: u64,
    pub malformed_returns: u64,
    #[serde(default)]
    pub ops_cbs_without_samples: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum BreakdownError {
    #[error("session contains no events")]
    EmptySession,
}

#[derive(Default)]
struct CallNode {
    qualname: String,
    class_name: String,
    parent: Option<u64>,
    children: Vec<u64>,
    gpu_ns_self: u64,
    eval_count: u64,
    cb_count: u64,
}

struct EvalInterval {
    t_in: u64,
    t_out: u64,
    stack: Vec<u64>,
}

pub fn compute(events: impl IntoIterator<Item = Event>) -> Result<ModuleBreakdown, BreakdownError> {
    let events: Vec<Event> = events.into_iter().collect();
    if events.is_empty() {
        return Err(BreakdownError::EmptySession);
    }

    // 1. Index module calls; track unclosed Entered and orphan Returned.
    let mut calls: HashMap<u64, CallNode> = HashMap::new();
    let mut open_calls: Vec<u64> = Vec::new();
    let mut malformed_returns: u64 = 0;
    for ev in &events {
        match &ev.payload {
            Payload::ModuleEntered {
                module_call_id,
                qualname,
                class_name,
                parent_call_id,
                ..
            } => {
                let node = CallNode {
                    qualname: qualname.clone(),
                    class_name: class_name.clone(),
                    parent: *parent_call_id,
                    ..Default::default()
                };
                if let Some(p) = parent_call_id {
                    if let Some(parent) = calls.get_mut(p) {
                        parent.children.push(*module_call_id);
                    }
                }
                calls.insert(*module_call_id, node);
                open_calls.push(*module_call_id);
            }
            Payload::ModuleReturned { module_call_id } => {
                if let Some(pos) = open_calls.iter().rposition(|c| c == module_call_id) {
                    open_calls.remove(pos);
                } else if !calls.contains_key(module_call_id) {
                    malformed_returns += 1;
                }
            }
            _ => {}
        }
    }
    malformed_returns += open_calls.len() as u64;

    // 2. Pair MlxEvalEntered/Returned by call_id.
    let mut eval_entered: HashMap<u64, (u64, Vec<u64>)> = HashMap::new();
    let mut eval_intervals: Vec<EvalInterval> = Vec::new();
    for ev in &events {
        match &ev.payload {
            Payload::MlxEvalEntered {
                call_id,
                module_stack,
                ..
            } => {
                eval_entered.insert(*call_id, (ev.ts_mono_ns, module_stack.clone()));
            }
            Payload::MlxEvalReturned { call_id, .. } => {
                if let Some((t_in, stack)) = eval_entered.remove(call_id) {
                    eval_intervals.push(EvalInterval {
                        t_in,
                        t_out: ev.ts_mono_ns,
                        stack,
                    });
                }
            }
            _ => {}
        }
    }
    eval_intervals.sort_by_key(|e| e.t_in);

    // 3. Pair MetalCbCommitted/Completed by cb_id.
    let mut cb_commit_ts: HashMap<u64, u64> = HashMap::new();
    let mut cb_completed: Vec<(u64, u64)> = Vec::new(); // (commit_ts, in_flight_ns)
    for ev in &events {
        match &ev.payload {
            Payload::MetalCbCommitted { cb_id, .. } => {
                cb_commit_ts.insert(*cb_id, ev.ts_mono_ns);
            }
            Payload::MetalCbCompleted {
                cb_id,
                in_flight_ns,
                ..
            } => {
                if let Some(commit_ts) = cb_commit_ts.remove(cb_id) {
                    cb_completed.push((commit_ts, *in_flight_ns));
                }
            }
            _ => {}
        }
    }

    // 4. Attribute each CB to the eval whose interval contains the commit ts.
    let mut unscoped_gpu_ns: u64 = 0;
    let mut unmatched_cb_count: u64 = 0;
    let mut per_eval_gpu_ns: Vec<u64> = vec![0; eval_intervals.len()];
    let mut per_eval_cb_count: Vec<u64> = vec![0; eval_intervals.len()];
    for (commit_ts, ns) in &cb_completed {
        let idx = eval_intervals
            .iter()
            .position(|e| e.t_in <= *commit_ts && *commit_ts <= e.t_out);
        match idx {
            Some(i) => {
                per_eval_gpu_ns[i] += *ns;
                per_eval_cb_count[i] += 1;
            }
            None => {
                unscoped_gpu_ns += *ns;
                unmatched_cb_count += 1;
            }
        }
    }

    // 5. Attribute each eval's gpu_ns to the leaf of its module stack.
    let mut unscoped_eval_count: u64 = 0;
    let mut unscoped_cb_count_from_evals: u64 = 0;
    for (i, eval) in eval_intervals.iter().enumerate() {
        let gpu = per_eval_gpu_ns[i];
        let cbs = per_eval_cb_count[i];
        if let Some(leaf) = eval.stack.last() {
            if let Some(node) = calls.get_mut(leaf) {
                node.gpu_ns_self += gpu;
                node.eval_count += 1;
                node.cb_count += cbs;
                continue;
            }
        }
        unscoped_gpu_ns += gpu;
        unscoped_eval_count += 1;
        unscoped_cb_count_from_evals += cbs;
    }

    // 6. Build the output tree.
    fn build(cid: u64, calls: &HashMap<u64, CallNode>) -> ModuleBreakdown {
        let n = calls.get(&cid).expect("call must exist");
        let mut children: Vec<ModuleBreakdown> =
            n.children.iter().map(|c| build(*c, calls)).collect();
        let subtree: u64 = n.gpu_ns_self + children.iter().map(|c| c.gpu_ns_subtree).sum::<u64>();
        children.sort_by_key(|b| std::cmp::Reverse(b.gpu_ns_subtree));
        ModuleBreakdown {
            qualname: n.qualname.clone(),
            class_name: n.class_name.clone(),
            calls: 1,
            gpu_ns_self: n.gpu_ns_self,
            gpu_ns_subtree: subtree,
            eval_count: n.eval_count,
            cb_count: n.cb_count,
            children,
            ops: vec![],
            diagnostics: None,
        }
    }

    let roots: Vec<u64> = calls
        .iter()
        .filter(|(_, n)| n.parent.is_none())
        .map(|(k, _)| *k)
        .collect();
    let mut root_children: Vec<ModuleBreakdown> = roots.iter().map(|r| build(*r, &calls)).collect();
    let total_subtree: u64 = root_children.iter().map(|c| c.gpu_ns_subtree).sum();
    let grand_total = total_subtree + unscoped_gpu_ns;
    if unscoped_gpu_ns > 0 || unscoped_eval_count > 0 || unmatched_cb_count > 0 {
        root_children.push(ModuleBreakdown {
            qualname: UNSCOPED.into(),
            class_name: String::new(),
            calls: 0,
            gpu_ns_self: unscoped_gpu_ns,
            gpu_ns_subtree: unscoped_gpu_ns,
            eval_count: unscoped_eval_count,
            cb_count: unmatched_cb_count + unscoped_cb_count_from_evals,
            children: vec![],
            ops: vec![],
            diagnostics: None,
        });
    }
    root_children.sort_by_key(|b| std::cmp::Reverse(b.gpu_ns_subtree));

    Ok(ModuleBreakdown {
        qualname: "<root>".into(),
        class_name: String::new(),
        calls: 0,
        gpu_ns_self: 0,
        gpu_ns_subtree: grand_total,
        eval_count: 0,
        cb_count: 0,
        children: root_children,
        ops: vec![],
        diagnostics: Some(Diagnostics {
            unscoped_gpu_ns,
            unmatched_cb_count,
            malformed_returns,
            ops_cbs_without_samples: 0,
        }),
    })
}

/// Render a flat table sorted by gpu_ns_subtree descending.
pub fn render_table(root: &ModuleBreakdown, top: usize, max_depth: u16) -> String {
    let total = root.gpu_ns_subtree.max(1);
    let mut rows: Vec<(u16, &ModuleBreakdown)> = Vec::new();
    fn walk<'a>(
        n: &'a ModuleBreakdown,
        depth: u16,
        max_depth: u16,
        out: &mut Vec<(u16, &'a ModuleBreakdown)>,
    ) {
        if depth > max_depth {
            return;
        }
        out.push((depth, n));
        for c in &n.children {
            walk(c, depth + 1, max_depth, out);
        }
    }
    for c in &root.children {
        walk(c, 0, max_depth, &mut rows);
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.1.gpu_ns_subtree));
    rows.truncate(top);

    let mut out = String::new();
    out.push_str(&format!(
        "{:<48} {:>8} {:>14} {:>14} {:>6}\n",
        "qualname", "calls", "gpu_self_us", "gpu_subtree_us", "pct"
    ));
    out.push_str(&"-".repeat(94));
    out.push('\n');
    for (depth, n) in rows {
        let indent = "  ".repeat(depth as usize);
        let name = format!("{indent}{}", n.qualname);
        let pct = (n.gpu_ns_subtree as f64 / total as f64) * 100.0;
        out.push_str(&format!(
            "{:<48} {:>8} {:>14.3} {:>14.3} {:>5.1}%\n",
            truncate(&name, 48),
            n.calls,
            n.gpu_ns_self as f64 / 1000.0,
            n.gpu_ns_subtree as f64 / 1000.0,
            pct,
        ));
    }
    if let Some(d) = &root.diagnostics {
        out.push_str(&format!(
            "\ndiagnostics: unscoped_gpu_us={:.3} unmatched_cb={} malformed_returns={}\n",
            d.unscoped_gpu_ns as f64 / 1000.0,
            d.unmatched_cb_count,
            d.malformed_returns,
        ));
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max - 1).collect();
        format!("{truncated}...")
    }
}

/// Chrome Trace Event Format: array of "complete" (ph=X) events.
/// Time unit: microseconds. Hierarchy is encoded via `tid`=depth.
pub fn render_chrome_trace(root: &ModuleBreakdown) -> String {
    let mut events: Vec<serde_json::Value> = Vec::new();
    let mut cursor_us: u64 = 0;
    fn walk(
        n: &ModuleBreakdown,
        depth: u16,
        cursor_us: &mut u64,
        events: &mut Vec<serde_json::Value>,
    ) {
        if n.qualname == "<root>" {
            for c in &n.children {
                walk(c, depth, cursor_us, events);
            }
            return;
        }
        let dur_us = (n.gpu_ns_subtree / 1000).max(1);
        let start = *cursor_us;
        events.push(serde_json::json!({
            "name": n.qualname,
            "cat": n.class_name,
            "ph": "X",
            "ts": start,
            "dur": dur_us,
            "pid": 0,
            "tid": depth,
            "args": {
                "calls": n.calls,
                "gpu_self_us": n.gpu_ns_self / 1000,
                "eval_count": n.eval_count,
                "cb_count": n.cb_count,
            }
        }));
        let mut child_cursor = start;
        for c in &n.children {
            walk(c, depth + 1, &mut child_cursor, events);
        }
        *cursor_us = start + dur_us;
    }
    walk(root, 0, &mut cursor_us, &mut events);
    serde_json::json!({
        "traceEvents": events,
        "displayTimeUnit": "us",
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{Payload, Source};
    use uuid::Uuid;

    fn ev(seq: u64, ts: u64, payload: Payload, source: Source) -> Event {
        Event {
            ts_mono_ns: ts,
            ts_wall_ns: ts,
            session_id: Uuid::nil(),
            source,
            pid: None,
            seq,
            payload,
        }
    }

    fn find_child<'a>(root: &'a ModuleBreakdown, qualname: &str) -> &'a ModuleBreakdown {
        root.children
            .iter()
            .find(|c| c.qualname == qualname)
            .unwrap_or_else(|| panic!("missing child {qualname}"))
    }

    #[test]
    fn empty_events_errors() {
        let r = compute(Vec::<Event>::new());
        assert!(matches!(r, Err(BreakdownError::EmptySession)));
    }

    #[test]
    fn top_level_call_goes_to_unscoped() {
        let evs = vec![
            ev(
                1,
                100,
                Payload::MlxEvalEntered {
                    call_id: 1,
                    array_count: 1,
                    stream: "gpu".into(),
                    module_stack: vec![],
                },
                Source::PythonSidecar,
            ),
            ev(
                2,
                110,
                Payload::MetalCbCommitted {
                    cb_id: 9,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
                Source::MetalHook,
            ),
            ev(
                3,
                120,
                Payload::MetalCbCompleted {
                    cb_id: 9,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 500,
                },
                Source::MetalHook,
            ),
            ev(
                4,
                200,
                Payload::MlxEvalReturned {
                    call_id: 1,
                    duration_ns: 100,
                    was_async: false,
                },
                Source::PythonSidecar,
            ),
        ];
        let r = compute(evs).unwrap();
        let unscoped = find_child(&r, UNSCOPED);
        assert_eq!(unscoped.gpu_ns_self, 500);
        assert_eq!(r.diagnostics.as_ref().unwrap().unscoped_gpu_ns, 500);
        assert_eq!(unscoped.cb_count, 1);
        assert_eq!(unscoped.eval_count, 1);
    }

    #[test]
    fn single_module_attributes_its_gpu_ns() {
        let evs = vec![
            ev(
                1,
                50,
                Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 1,
                    qualname: "Linear".into(),
                    class_name: "Linear".into(),
                    parent_call_id: None,
                    depth: 0,
                },
                Source::PythonSidecar,
            ),
            ev(
                2,
                100,
                Payload::MlxEvalEntered {
                    call_id: 7,
                    array_count: 1,
                    stream: "gpu".into(),
                    module_stack: vec![1],
                },
                Source::PythonSidecar,
            ),
            ev(
                3,
                110,
                Payload::MetalCbCommitted {
                    cb_id: 9,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
                Source::MetalHook,
            ),
            ev(
                4,
                120,
                Payload::MetalCbCompleted {
                    cb_id: 9,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 700,
                },
                Source::MetalHook,
            ),
            ev(
                5,
                200,
                Payload::MlxEvalReturned {
                    call_id: 7,
                    duration_ns: 100,
                    was_async: false,
                },
                Source::PythonSidecar,
            ),
            ev(
                6,
                210,
                Payload::ModuleReturned { module_call_id: 1 },
                Source::PythonSidecar,
            ),
        ];
        let r = compute(evs).unwrap();
        let lin = find_child(&r, "Linear");
        assert_eq!(lin.gpu_ns_self, 700);
        assert_eq!(lin.gpu_ns_subtree, 700);
        assert_eq!(lin.eval_count, 1);
        assert_eq!(lin.cb_count, 1);
    }

    #[test]
    fn hierarchy_sums_subtree() {
        let evs = vec![
            ev(
                1,
                10,
                Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 1,
                    qualname: "Block".into(),
                    class_name: "Block".into(),
                    parent_call_id: None,
                    depth: 0,
                },
                Source::PythonSidecar,
            ),
            ev(
                2,
                20,
                Payload::ModuleEntered {
                    module_call_id: 2,
                    module_def_id: 2,
                    qualname: "Linear".into(),
                    class_name: "Linear".into(),
                    parent_call_id: Some(1),
                    depth: 1,
                },
                Source::PythonSidecar,
            ),
            ev(
                3,
                100,
                Payload::MlxEvalEntered {
                    call_id: 7,
                    array_count: 1,
                    stream: "gpu".into(),
                    module_stack: vec![1, 2],
                },
                Source::PythonSidecar,
            ),
            ev(
                4,
                110,
                Payload::MetalCbCommitted {
                    cb_id: 9,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
                Source::MetalHook,
            ),
            ev(
                5,
                120,
                Payload::MetalCbCompleted {
                    cb_id: 9,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 1000,
                },
                Source::MetalHook,
            ),
            ev(
                6,
                200,
                Payload::MlxEvalReturned {
                    call_id: 7,
                    duration_ns: 100,
                    was_async: false,
                },
                Source::PythonSidecar,
            ),
            ev(
                7,
                210,
                Payload::ModuleReturned { module_call_id: 2 },
                Source::PythonSidecar,
            ),
            ev(
                8,
                220,
                Payload::ModuleReturned { module_call_id: 1 },
                Source::PythonSidecar,
            ),
        ];
        let r = compute(evs).unwrap();
        let block = find_child(&r, "Block");
        assert_eq!(block.gpu_ns_self, 0);
        assert_eq!(block.gpu_ns_subtree, 1000);
        let lin = block
            .children
            .iter()
            .find(|c| c.qualname == "Linear")
            .unwrap();
        assert_eq!(lin.gpu_ns_self, 1000);
    }

    #[test]
    fn unmatched_cb_is_unscoped() {
        let evs = vec![
            ev(
                1,
                100,
                Payload::MetalCbCommitted {
                    cb_id: 1,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
                Source::MetalHook,
            ),
            ev(
                2,
                110,
                Payload::MetalCbCompleted {
                    cb_id: 1,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 42,
                },
                Source::MetalHook,
            ),
        ];
        let r = compute(evs).unwrap();
        let d = r.diagnostics.as_ref().unwrap();
        assert_eq!(d.unmatched_cb_count, 1);
        assert_eq!(d.unscoped_gpu_ns, 42);
    }

    #[test]
    fn returned_without_entered_is_malformed() {
        let evs = vec![ev(
            1,
            1,
            Payload::ModuleReturned { module_call_id: 99 },
            Source::PythonSidecar,
        )];
        let r = compute(evs).unwrap();
        assert_eq!(r.diagnostics.as_ref().unwrap().malformed_returns, 1);
    }

    #[test]
    fn entered_without_returned_is_malformed() {
        let evs = vec![ev(
            1,
            1,
            Payload::ModuleEntered {
                module_call_id: 1,
                module_def_id: 1,
                qualname: "X".into(),
                class_name: "X".into(),
                parent_call_id: None,
                depth: 0,
            },
            Source::PythonSidecar,
        )];
        let r = compute(evs).unwrap();
        assert_eq!(r.diagnostics.as_ref().unwrap().malformed_returns, 1);
    }

    fn sample_breakdown() -> ModuleBreakdown {
        ModuleBreakdown {
            qualname: "<root>".into(),
            class_name: "".into(),
            calls: 0,
            gpu_ns_self: 0,
            gpu_ns_subtree: 1500,
            eval_count: 0,
            cb_count: 0,
            children: vec![ModuleBreakdown {
                qualname: "Block".into(),
                class_name: "Block".into(),
                calls: 1,
                gpu_ns_self: 0,
                gpu_ns_subtree: 1500,
                eval_count: 0,
                cb_count: 0,
                children: vec![ModuleBreakdown {
                    qualname: "Linear".into(),
                    class_name: "Linear".into(),
                    calls: 1,
                    gpu_ns_self: 1500,
                    gpu_ns_subtree: 1500,
                    eval_count: 1,
                    cb_count: 1,
                    children: vec![],
                    ops: vec![],
                    diagnostics: None,
                }],
                ops: vec![],
                diagnostics: None,
            }],
            ops: vec![],
            diagnostics: Some(Diagnostics::default()),
        }
    }

    #[test]
    fn render_table_shows_qualnames_and_durations() {
        let s = render_table(&sample_breakdown(), 10, 6);
        assert!(s.contains("Block"));
        assert!(s.contains("Linear"));
        assert!(s.contains("1.500")); // 1500 ns formatted as us
    }

    #[test]
    fn render_chrome_trace_is_valid_json_with_complete_events() {
        let json = render_chrome_trace(&sample_breakdown());
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = parsed["traceEvents"].as_array().unwrap();
        assert!(arr.iter().any(|e| e["name"] == "Linear" && e["ph"] == "X"));
        assert_eq!(parsed["displayTimeUnit"], "us");
    }
}

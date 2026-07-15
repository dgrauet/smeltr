//! Module-level GPU breakdown computed from a session's events.

use serde::{Deserialize, Serialize};
use smeltr_core::event::{Event, OpSample, Payload};
use std::collections::HashMap;

/// Reserved qualname for time not attributable to any module call.
pub const UNSCOPED: &str = "<unscoped>";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct OpAttribution {
    pub name: String,
    pub gpu_ns: u64,
    pub count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub fields: std::collections::BTreeMap<String, smeltr_core::event::FieldValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Diagnostics {
    pub unscoped_gpu_ns: u64,
    pub unmatched_cb_count: u64,
    pub malformed_returns: u64,
    #[serde(default)]
    pub ops_cbs_without_samples: u64,
    /// CBs attributed via the #131 scope-window fallback (no eval window
    /// contained their commit).
    #[serde(default)]
    pub cbs_scope_attributed: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum BreakdownError {
    #[error("session contains no events")]
    EmptySession,
}

/// Aggregation bucket for ops keyed by op name: (gpu_ns, count, symbol).
/// `symbol` is set on first non-None occurrence; same kernel name is
/// assumed to come from the same PSO and thus same symbol.
type OpAgg = (u64, u64, Option<String>);

#[derive(Default)]
struct CallNode {
    qualname: String,
    class_name: String,
    parent: Option<u64>,
    children: Vec<u64>,
    gpu_ns_self: u64,
    eval_count: u64,
    cb_count: u64,
    ops_buf: HashMap<String, OpAgg>,
    fields: std::collections::BTreeMap<String, smeltr_core::event::FieldValue>,
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
    // Also record each call's [entered, returned] wall window for the #131
    // scope fallback below.
    let mut calls: HashMap<u64, CallNode> = HashMap::new();
    let mut open_calls: Vec<u64> = Vec::new();
    let mut malformed_returns: u64 = 0;
    struct ModuleWindow {
        t_in: u64,
        t_out: u64,
        call_id: u64,
    }
    let mut module_windows: Vec<ModuleWindow> = Vec::new();
    let mut open_window_idx: HashMap<u64, usize> = HashMap::new();
    let last_event_ts = events.last().map(|e| e.ts_mono_ns).unwrap_or(0);
    for ev in &events {
        match &ev.payload {
            Payload::ModuleEntered {
                module_call_id,
                qualname,
                class_name,
                parent_call_id,
                fields,
                ..
            } => {
                let node = CallNode {
                    qualname: qualname.clone(),
                    class_name: class_name.clone(),
                    parent: *parent_call_id,
                    fields: fields.clone(),
                    ..Default::default()
                };
                if let Some(p) = parent_call_id {
                    if let Some(parent) = calls.get_mut(p) {
                        parent.children.push(*module_call_id);
                    }
                }
                calls.insert(*module_call_id, node);
                open_calls.push(*module_call_id);
                open_window_idx.insert(*module_call_id, module_windows.len());
                module_windows.push(ModuleWindow {
                    t_in: ev.ts_mono_ns,
                    // Closed on ModuleReturned; a never-returned call (e.g.
                    // aborted run) stays open until the end of the session.
                    t_out: last_event_ts,
                    call_id: *module_call_id,
                });
            }
            Payload::ModuleReturned { module_call_id } => {
                if let Some(pos) = open_calls.iter().rposition(|c| c == module_call_id) {
                    open_calls.remove(pos);
                    if let Some(&i) = open_window_idx.get(module_call_id) {
                        module_windows[i].t_out = ev.ts_mono_ns;
                    }
                } else if !calls.contains_key(module_call_id) {
                    malformed_returns += 1;
                }
            }
            _ => {}
        }
    }
    malformed_returns += open_calls.len() as u64;

    // 2. Pair MlxEvalEntered/Returned by call_id.
    //
    // MLX 0.31+ uses async GPU scheduling: mx.eval() returns quickly (< 10 ms)
    // after merely queuing GPU work; the Metal CBs are committed by the driver
    // thread up to ~500 ms later. To keep those CBs inside their attribution
    // window we extend t_out by ASYNC_GRACE_NS when was_async=true.
    const ASYNC_GRACE_NS: u64 = 500_000_000; // 500 ms
    let mut compute_entered: HashMap<u64, (u64, Vec<u64>)> = HashMap::new();
    let mut compute_intervals: Vec<EvalInterval> = Vec::new();
    for ev in &events {
        match &ev.payload {
            Payload::MlxEvalEntered {
                call_id,
                module_stack,
                ..
            } => {
                compute_entered.insert(*call_id, (ev.ts_mono_ns, module_stack.clone()));
            }
            Payload::MlxEvalReturned {
                call_id, was_async, ..
            } => {
                if let Some((t_in, stack)) = compute_entered.remove(call_id) {
                    let t_out = if *was_async {
                        ev.ts_mono_ns.saturating_add(ASYNC_GRACE_NS)
                    } else {
                        ev.ts_mono_ns
                    };
                    compute_intervals.push(EvalInterval { t_in, t_out, stack });
                }
            }
            _ => {}
        }
    }
    let eval_intervals = {
        let mut v = compute_intervals;
        v.sort_by_key(|e| e.t_in);
        v
    };

    // 3. Pair MetalCbCommitted/Completed and attach each MetalCbOps to the
    // completion it belongs to — chronologically, NOT via a session-wide
    // cb_id index: cb_id is the CB *pointer* and Metal recycles CB
    // allocations, so one id spans many lifetimes (#127 — the old index
    // attributed the last lifetime's ops to every completion sharing the
    // pointer, multiplying op time ~×29 on a real run). The hook emits
    // CbOps immediately after the matching CbCompleted.
    let mut cb_commit_ts: HashMap<u64, u64> = HashMap::new();
    // (cb_id, commit_ts, in_flight_ns, ops)
    let mut cb_completed: Vec<(u64, u64, u64, Option<Vec<OpSample>>)> = Vec::new();
    let mut last_completed_idx: HashMap<u64, usize> = HashMap::new();
    let mut seen_any_cb_ops = false;
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
                    last_completed_idx.insert(*cb_id, cb_completed.len());
                    cb_completed.push((*cb_id, commit_ts, *in_flight_ns, None));
                }
            }
            Payload::MetalCbOps { cb_id, ops } => {
                seen_any_cb_ops = true;
                if let Some(&i) = last_completed_idx.get(cb_id) {
                    cb_completed[i].3 = Some(ops.clone());
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
    let mut per_eval_ops: Vec<HashMap<String, OpAgg>> =
        (0..eval_intervals.len()).map(|_| HashMap::new()).collect();
    let mut unscoped_ops: HashMap<String, OpAgg> = HashMap::new();
    let mut ops_cbs_without_samples: u64 = 0;
    let mut no_eval_window: Vec<(u64, u64, Option<&Vec<OpSample>>)> = Vec::new();
    for (_cb_id, commit_ts, ns, ops_for_cb) in &cb_completed {
        let idx = eval_intervals
            .iter()
            .position(|e| e.t_in <= *commit_ts && *commit_ts <= e.t_out);
        let ops_for_cb = ops_for_cb.as_ref();
        if seen_any_cb_ops && ops_for_cb.is_none() {
            ops_cbs_without_samples += 1;
        }
        // A CB's GPU contribution is the sum of its op times (exact
        // stage-sampled, or queue-clamped pro-rata) — NOT its in_flight_ns:
        // pipelined CBs overlap, so summing in_flight over-counts by
        // ~queue-depth x (#136: 6402 s shown for a 330 s run). When op
        // capture is on, a CB without ops (blit/host copies, empty CBs)
        // contributes 0 — its overlapped in_flight is exactly the
        // over-count (measured 11x on a real session) and it is already
        // counted in ops_cbs_without_samples. in_flight is the fallback
        // only when op capture is off for the whole session.
        let ns = &match ops_for_cb {
            Some(ops) => ops.iter().map(|o| o.gpu_ns).sum::<u64>(),
            None if seen_any_cb_ops => 0,
            None => *ns,
        };
        match idx {
            Some(i) => {
                per_eval_gpu_ns[i] += *ns;
                per_eval_cb_count[i] += 1;
                if let Some(ops) = ops_for_cb {
                    for op in ops {
                        let e = per_eval_ops[i]
                            .entry(op.name.clone())
                            .or_insert((0, 0, None));
                        e.0 += op.gpu_ns;
                        e.1 += op.count as u64;
                        if e.2.is_none() {
                            e.2 = op.symbol.clone();
                        }
                    }
                }
            }
            None => {
                no_eval_window.push((*commit_ts, *ns, ops_for_cb));
            }
        }
    }

    // 4.5. Scope fallback (#131): lazy workloads barely call mx.eval (ERNIE:
    // 2 calls in a whole run), leaving almost every CB without an eval
    // window — 99.8 % of GPU time used to land in <unscoped>. Attribute
    // those CBs to the innermost scope/module window open at commit time
    // (same ASYNC_GRACE tail as eval windows); only CBs outside every
    // window stay unscoped. Sweep: windows sorted by t_in, CBs by commit
    // ts, a stack of open windows (module calls nest on the Python side).
    let mut fallback: HashMap<u64, (u64, u64, HashMap<String, OpAgg>)> = HashMap::new();
    let mut cbs_scope_attributed: u64 = 0;
    module_windows.sort_by_key(|w| w.t_in);
    no_eval_window.sort_by_key(|(ts, _, _)| *ts);
    let mut next_window = 0usize;
    let mut stack: Vec<&ModuleWindow> = Vec::new();
    for (commit_ts, ns, ops_for_cb) in no_eval_window {
        while next_window < module_windows.len() && module_windows[next_window].t_in <= commit_ts {
            stack.push(&module_windows[next_window]);
            next_window += 1;
        }
        while let Some(top) = stack.last() {
            if top.t_out.saturating_add(ASYNC_GRACE_NS) < commit_ts {
                stack.pop();
            } else {
                break;
            }
        }
        match stack.last() {
            Some(win) => {
                cbs_scope_attributed += 1;
                let slot = fallback.entry(win.call_id).or_default();
                slot.0 += ns;
                slot.1 += 1;
                if let Some(ops) = ops_for_cb {
                    for op in ops {
                        let e = slot.2.entry(op.name.clone()).or_insert((0, 0, None));
                        e.0 += op.gpu_ns;
                        e.1 += op.count as u64;
                        if e.2.is_none() {
                            e.2 = op.symbol.clone();
                        }
                    }
                }
            }
            None => {
                unscoped_gpu_ns += ns;
                unmatched_cb_count += 1;
                if let Some(ops) = ops_for_cb {
                    for op in ops {
                        let e = unscoped_ops.entry(op.name.clone()).or_insert((0, 0, None));
                        e.0 += op.gpu_ns;
                        e.1 += op.count as u64;
                        if e.2.is_none() {
                            e.2 = op.symbol.clone();
                        }
                    }
                }
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
                for (name, (ns, c, sym)) in per_eval_ops[i].drain() {
                    let e = node.ops_buf.entry(name).or_insert((0, 0, None));
                    e.0 += ns;
                    e.1 += c;
                    if e.2.is_none() {
                        e.2 = sym;
                    }
                }
                continue;
            }
        }
        unscoped_gpu_ns += gpu;
        unscoped_eval_count += 1;
        unscoped_cb_count_from_evals += cbs;
        for (name, (ns, c, sym)) in per_eval_ops[i].drain() {
            let e = unscoped_ops.entry(name).or_insert((0, 0, None));
            e.0 += ns;
            e.1 += c;
            if e.2.is_none() {
                e.2 = sym;
            }
        }
    }

    // 5.5. Merge the scope-fallback attributions (#131) into their nodes.
    for (call_id, (gpu, cbs, ops)) in fallback {
        if let Some(node) = calls.get_mut(&call_id) {
            node.gpu_ns_self += gpu;
            node.cb_count += cbs;
            for (name, (ns, c, sym)) in ops {
                let e = node.ops_buf.entry(name).or_insert((0, 0, None));
                e.0 += ns;
                e.1 += c;
                if e.2.is_none() {
                    e.2 = sym;
                }
            }
        } else {
            unscoped_gpu_ns += gpu;
            unmatched_cb_count += cbs;
        }
    }

    // 6. Build the output tree.
    fn build(cid: u64, calls: &HashMap<u64, CallNode>) -> ModuleBreakdown {
        let n = calls.get(&cid).expect("call must exist");
        let mut children: Vec<ModuleBreakdown> =
            n.children.iter().map(|c| build(*c, calls)).collect();
        let subtree: u64 = n.gpu_ns_self + children.iter().map(|c| c.gpu_ns_subtree).sum::<u64>();
        children.sort_by_key(|b| std::cmp::Reverse(b.gpu_ns_subtree));
        let mut ops: Vec<OpAttribution> = n
            .ops_buf
            .iter()
            .map(|(name, (ns, c, sym))| {
                let kind = sym
                    .as_deref()
                    .and_then(crate::op_kinds::resolve_kind)
                    .map(str::to_string);
                OpAttribution {
                    name: name.clone(),
                    gpu_ns: *ns,
                    count: *c,
                    symbol: sym.clone(),
                    kind,
                }
            })
            .collect();
        ops.sort_by_key(|o| std::cmp::Reverse(o.gpu_ns));
        ModuleBreakdown {
            qualname: n.qualname.clone(),
            class_name: n.class_name.clone(),
            calls: 1,
            gpu_ns_self: n.gpu_ns_self,
            gpu_ns_subtree: subtree,
            eval_count: n.eval_count,
            cb_count: n.cb_count,
            children,
            ops,
            diagnostics: None,
            fields: n.fields.clone(),
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
        let mut unscoped_ops_vec: Vec<OpAttribution> = unscoped_ops
            .into_iter()
            .map(|(name, (ns, c, sym))| {
                let kind = sym
                    .as_deref()
                    .and_then(crate::op_kinds::resolve_kind)
                    .map(str::to_string);
                OpAttribution {
                    name,
                    gpu_ns: ns,
                    count: c,
                    symbol: sym,
                    kind,
                }
            })
            .collect();
        unscoped_ops_vec.sort_by_key(|o| std::cmp::Reverse(o.gpu_ns));
        root_children.push(ModuleBreakdown {
            qualname: UNSCOPED.into(),
            class_name: String::new(),
            calls: 0,
            gpu_ns_self: unscoped_gpu_ns,
            gpu_ns_subtree: unscoped_gpu_ns,
            eval_count: unscoped_eval_count,
            cb_count: unmatched_cb_count + unscoped_cb_count_from_evals,
            children: vec![],
            ops: unscoped_ops_vec,
            diagnostics: None,
            fields: Default::default(),
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
            ops_cbs_without_samples,
            cbs_scope_attributed,
        }),
        fields: Default::default(),
    })
}

/// Render a flat table sorted by gpu_ns_subtree descending.
pub fn render_table(
    root: &ModuleBreakdown,
    top: usize,
    max_depth: u16,
    top_ops: usize,
    show_ops: bool,
) -> String {
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
        if show_ops && !n.ops.is_empty() {
            for op in n.ops.iter().take(top_ops) {
                let op_indent = "  ".repeat((depth as usize) + 1);
                out.push_str(&format!(
                    "{:<48} {:>8} {:>14.3} {:>14} {:>5}\n",
                    truncate(&format!("{op_indent}\u{2514} op:{}", op.name), 48),
                    op.count,
                    op.gpu_ns as f64 / 1000.0,
                    "",
                    "",
                ));
            }
        }
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
        let mut op_cursor = start;
        for op in &n.ops {
            let dur_us = (op.gpu_ns / 1000).max(1);
            events.push(serde_json::json!({
                "name": op.name,
                "cat": "op",
                "ph": "X",
                "ts": op_cursor,
                "dur": dur_us,
                "pid": 0,
                "tid": depth + 1,
                "args": { "count": op.count, "gpu_ns": op.gpu_ns },
            }));
            op_cursor += dur_us;
        }
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

/// Re-aggregate one node's ops by resolved kind, falling back to the op name
/// when `kind` is None. `symbol` is dropped (a kind row spans many kernels);
/// `kind` is the first non-None in the bucket. Sorted by gpu_ns desc, tie by name.
fn regroup_ops_by_kind(ops: &[OpAttribution]) -> Vec<OpAttribution> {
    let mut agg: HashMap<String, (u64, u64, Option<String>)> = HashMap::new();
    for op in ops {
        let key = op.kind.clone().unwrap_or_else(|| op.name.clone());
        let e = agg.entry(key).or_insert((0, 0, None));
        e.0 += op.gpu_ns;
        e.1 += op.count;
        if e.2.is_none() {
            e.2 = op.kind.clone();
        }
    }
    let mut rows: Vec<OpAttribution> = agg
        .into_iter()
        .map(|(name, (gpu_ns, count, kind))| OpAttribution {
            name,
            gpu_ns,
            count,
            symbol: None,
            kind,
        })
        .collect();
    rows.sort_by(|a, b| b.gpu_ns.cmp(&a.gpu_ns).then(a.name.cmp(&b.name)));
    rows
}

/// Walk the tree; for `OpGroupBy::Kind`, replace every node's `ops` with the
/// per-node kind-regrouped version. No-op for `OpGroupBy::Name`.
pub fn apply_op_group_by(node: &mut ModuleBreakdown, group_by: OpGroupBy) {
    if let OpGroupBy::Kind = group_by {
        node.ops = regroup_ops_by_kind(&node.ops);
        for c in &mut node.children {
            apply_op_group_by(c, group_by);
        }
    }
}

/// Controls how ops are grouped in the flat summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpGroupBy {
    /// Group by op name (original behavior).
    Name,
    /// Group by resolved kind (falls back to op name when kind is None).
    Kind,
}

/// A single row in the flat op summary.
///
/// In Kind mode (`OpGroupBy::Kind`), `symbol` is always `None` because a kind
/// row spans many kernels with no single representative symbol. A kernel that
/// has no resolved kind falls back to using its own name as the key; in that
/// case `key == op name` and `kind == None`.
#[derive(Debug, Clone)]
pub struct OpFlatRow {
    pub key: String,
    pub gpu_ns: u64,
    pub count: u64,
    /// Representative kernel symbol (Name mode only; None in Kind mode).
    pub symbol: Option<String>,
    pub kind: Option<String>,
}

// key -> (gpu_ns, count, symbol, kind)
type OpAggMap = HashMap<String, (u64, u64, Option<String>, Option<String>)>;

/// Flatten all ops in the module tree into rows keyed by op name or resolved
/// kind. Returns ALL rows sorted by gpu_ns desc (no truncation — callers
/// compute totals over the full set, then truncate, so percentages stay
/// correct).
pub fn aggregate_ops_flat(root: &ModuleBreakdown, group_by: OpGroupBy) -> Vec<OpFlatRow> {
    let mut agg: OpAggMap = HashMap::new();
    fn walk(n: &ModuleBreakdown, group_by: OpGroupBy, agg: &mut OpAggMap) {
        for op in &n.ops {
            let key = match group_by {
                OpGroupBy::Name => op.name.clone(),
                OpGroupBy::Kind => op.kind.clone().unwrap_or_else(|| op.name.clone()),
            };
            let e = agg.entry(key).or_insert((0, 0, None, None));
            e.0 += op.gpu_ns;
            e.1 += op.count;
            if e.2.is_none() {
                e.2 = op.symbol.clone();
            }
            if e.3.is_none() {
                e.3 = op.kind.clone();
            }
        }
        for c in &n.children {
            walk(c, group_by, agg);
        }
    }
    walk(root, group_by, &mut agg);

    let mut rows: Vec<OpFlatRow> = agg
        .into_iter()
        .map(|(key, (gpu_ns, count, symbol, kind))| OpFlatRow {
            key,
            gpu_ns,
            count,
            // A kind row spans many kernels: no single representative symbol.
            symbol: match group_by {
                OpGroupBy::Name => symbol,
                OpGroupBy::Kind => None,
            },
            kind,
        })
        .collect();
    rows.sort_by(|a, b| b.gpu_ns.cmp(&a.gpu_ns).then(a.key.cmp(&b.key)));
    rows
}

/// Aggregate ops across all module leaves and return a flat table sorted by gpu_ns desc.
pub fn render_ops_flat(root: &ModuleBreakdown, group_by: OpGroupBy, top: usize) -> String {
    let rows = aggregate_ops_flat(root, group_by);
    let total: u64 = rows.iter().map(|r| r.gpu_ns).sum::<u64>().max(1);

    let mut out = String::new();
    out.push_str(&format!(
        "{:<32} {:>8} {:>14} {:>6}\n",
        "op", "count", "gpu_us", "pct"
    ));
    out.push_str(&"-".repeat(64));
    out.push('\n');
    for r in rows.iter().take(top) {
        let pct = (r.gpu_ns as f64 / total as f64) * 100.0;
        out.push_str(&format!(
            "{:<32} {:>8} {:>14.3} {:>5.1}%\n",
            r.key,
            r.count,
            r.gpu_ns as f64 / 1000.0,
            pct,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use smeltr_core::event::{OpSample, Payload, Source};
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

    fn op_sample(name: &str, gpu_ns: u64, count: u32) -> OpSample {
        OpSample {
            name: name.into(),
            symbol: None,
            gpu_ns,
            count,
        }
    }

    /// #127: Metal recycles CB allocations, so the same cb_id (pointer) is
    /// reused by successive CBs. Each lifetime's MetalCbOps must be
    /// attributed exactly once, to its own eval window — not the last
    /// lifetime's ops to every completion sharing the pointer.
    #[test]
    fn recycled_cb_ids_attribute_each_lifetime_once() {
        let mk_eval = |seq: u64, call_id: u64, t_in: u64, t_out: u64| {
            vec![
                ev(
                    seq,
                    t_in,
                    Payload::MlxEvalEntered {
                        call_id,
                        array_count: 1,
                        stream: "gpu".into(),
                        module_stack: vec![1],
                        stack_frames: vec![],
                    },
                    Source::PythonSidecar,
                ),
                ev(
                    seq + 1,
                    t_out,
                    Payload::MlxEvalReturned {
                        call_id,
                        duration_ns: t_out - t_in,
                        was_async: false,
                    },
                    Source::PythonSidecar,
                ),
            ]
        };
        let mk_cb = |seq: u64, ts: u64, cb_id: u64, op: &str, gpu_ns: u64| {
            vec![
                ev(
                    seq,
                    ts,
                    Payload::MetalCbCommitted {
                        cb_id,
                        queue_id: 1,
                        queue_depth: 1,
                        label: None,
                    },
                    Source::MetalHook,
                ),
                ev(
                    seq + 1,
                    ts + 5,
                    Payload::MetalCbCompleted {
                        cb_id,
                        queue_id: 1,
                        status: 4,
                        error_code: None,
                        error_domain: None,
                        in_flight_ns: 5,
                    },
                    Source::MetalHook,
                ),
                ev(
                    seq + 2,
                    ts + 6,
                    Payload::MetalCbOps {
                        cb_id,
                        ops: vec![op_sample(op, gpu_ns, 1)],
                    },
                    Source::MetalHook,
                ),
            ]
        };
        let mut events = vec![ev(
            1,
            0,
            Payload::ModuleEntered {
                module_call_id: 1,
                module_def_id: 1,
                qualname: "m".into(),
                class_name: "M".into(),
                parent_call_id: None,
                depth: 0,
                fields: Default::default(),
            },
            Source::PythonSidecar,
        )];
        // Two eval windows; ONE cb_id (0x9) recycled across both, with
        // different ops in each lifetime.
        events.extend(mk_eval(10, 7, 100, 200));
        events.extend(mk_cb(20, 110, 0x9, "OpFirst", 111));
        events.extend(mk_eval(30, 8, 300, 400));
        events.extend(mk_cb(40, 310, 0x9, "OpSecond", 222));
        events.push(ev(
            50,
            500,
            Payload::ModuleReturned { module_call_id: 1 },
            Source::PythonSidecar,
        ));

        let root = compute(events).unwrap();
        let m = find_child(&root, "m");
        let first = m.ops.iter().find(|o| o.name == "OpFirst");
        let second = m.ops.iter().find(|o| o.name == "OpSecond");
        assert!(
            first.is_some_and(|o| o.gpu_ns == 111 && o.count == 1),
            "first lifetime's ops must be attributed once: {:?}",
            m.ops
        );
        assert!(
            second.is_some_and(|o| o.gpu_ns == 222 && o.count == 1),
            "second lifetime's ops must be attributed once: {:?}",
            m.ops
        );
    }

    /// #131: lazy workloads (ERNIE: 2 mx.eval calls in a whole run) have
    /// almost no eval windows, so 99.8 % of GPU time fell to <unscoped>.
    /// CBs whose commit matches no eval window must fall back to the
    /// innermost open scope/module window instead.
    #[test]
    fn cb_without_eval_window_attributes_to_innermost_scope() {
        let mut events = vec![
            ev(
                1,
                10_000_000_000,
                Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 1,
                    qualname: "generate".into(),
                    class_name: "scope".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields: Default::default(),
                },
                Source::PythonSidecar,
            ),
            ev(
                2,
                20_000_000_000,
                Payload::ModuleEntered {
                    module_call_id: 2,
                    module_def_id: 2,
                    qualname: "DiT".into(),
                    class_name: "M".into(),
                    parent_call_id: Some(1),
                    depth: 1,
                    fields: Default::default(),
                },
                Source::PythonSidecar,
            ),
            ev(
                3,
                21_000_000_000,
                Payload::ModuleReturned { module_call_id: 2 },
                Source::PythonSidecar,
            ),
        ];
        // CB A commits inside the inner DiT window.
        events.extend(cb_lifecycle(10, 20_500_000_000, 0xa, "OpInner", 111));
        // CB B commits later, only the outer scope is open.
        events.extend(cb_lifecycle(20, 50_000_000_000, 0xb, "OpOuter", 222));
        events.push(ev(
            30,
            100_000_000_000,
            Payload::ModuleReturned { module_call_id: 1 },
            Source::PythonSidecar,
        ));
        // CB C commits after every window (+ grace): stays unscoped.
        events.extend(cb_lifecycle(40, 200_000_000_000, 0xc, "OpNowhere", 333));

        let root = compute(events).unwrap();
        let scope = find_child(&root, "generate");
        let dit = find_child(scope, "DiT");
        assert!(
            dit.ops
                .iter()
                .any(|o| o.name == "OpInner" && o.gpu_ns == 111),
            "inner CB must land on the innermost window: {:?}",
            dit.ops
        );
        assert!(
            scope
                .ops
                .iter()
                .any(|o| o.name == "OpOuter" && o.gpu_ns == 222),
            "outer CB must land on the enclosing scope: {:?}",
            scope.ops
        );
        let diag = root.diagnostics.as_ref().unwrap();
        assert_eq!(diag.unmatched_cb_count, 1, "only CB C stays unscoped");
    }

    /// Eval windows keep priority: a CB inside both an eval window and a
    /// scope window is attributed once, via the eval path.
    #[test]
    fn eval_window_takes_priority_over_scope_fallback() {
        let mut events = vec![ev(
            1,
            10_000_000_000,
            Payload::ModuleEntered {
                module_call_id: 1,
                module_def_id: 1,
                qualname: "generate".into(),
                class_name: "scope".into(),
                parent_call_id: None,
                depth: 0,
                fields: Default::default(),
            },
            Source::PythonSidecar,
        )];
        events.push(ev(
            2,
            30_000_000_000,
            Payload::MlxEvalEntered {
                call_id: 7,
                array_count: 1,
                stream: "gpu".into(),
                module_stack: vec![1],
                stack_frames: vec![],
            },
            Source::PythonSidecar,
        ));
        events.extend(cb_lifecycle(10, 30_500_000_000, 0xa, "OpEval", 111));
        events.push(ev(
            20,
            31_000_000_000,
            Payload::MlxEvalReturned {
                call_id: 7,
                duration_ns: 1_000_000_000,
                was_async: false,
            },
            Source::PythonSidecar,
        ));
        events.push(ev(
            30,
            100_000_000_000,
            Payload::ModuleReturned { module_call_id: 1 },
            Source::PythonSidecar,
        ));

        let root = compute(events).unwrap();
        let scope = find_child(&root, "generate");
        let total: u64 = scope.ops.iter().map(|o| o.gpu_ns).sum();
        assert_eq!(total, 111, "attributed exactly once: {:?}", scope.ops);
        assert_eq!(
            scope.eval_count, 1,
            "attribution went through the eval path"
        );
    }

    /// #136: a window's gpu_ns must be the sum of its CBs' op times (exact
    /// since #124/#126), not of their in_flight_ns — overlapping pipelined
    /// CBs each count the same wall seconds (6402 s shown for a 330 s run).
    #[test]
    fn window_gpu_ns_sums_op_times_not_overlapping_in_flight() {
        let mut events = vec![
            ev(
                1,
                50,
                Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 1,
                    qualname: "m".into(),
                    class_name: "M".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields: Default::default(),
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
                    stack_frames: vec![],
                },
                Source::PythonSidecar,
            ),
        ];
        // Two deeply-pipelined CBs: in_flight 1000 each (overlapping), but
        // actual op time 111 and 222.
        for (i, (cb_id, op, gpu)) in [(0xa_u64, "OpA", 111_u64), (0xb, "OpB", 222)]
            .into_iter()
            .enumerate()
        {
            let seq = 10 + 10 * i as u64;
            let ts = 110 + i as u64;
            events.push(ev(
                seq,
                ts,
                Payload::MetalCbCommitted {
                    cb_id,
                    queue_id: 1,
                    queue_depth: 2,
                    label: None,
                },
                Source::MetalHook,
            ));
            events.push(ev(
                seq + 1,
                ts + 1000,
                Payload::MetalCbCompleted {
                    cb_id,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 1000,
                },
                Source::MetalHook,
            ));
            events.push(ev(
                seq + 2,
                ts + 1001,
                Payload::MetalCbOps {
                    cb_id,
                    ops: vec![op_sample(op, gpu, 1)],
                },
                Source::MetalHook,
            ));
        }
        events.push(ev(
            40,
            2000,
            Payload::MlxEvalReturned {
                call_id: 7,
                duration_ns: 1900,
                was_async: false,
            },
            Source::PythonSidecar,
        ));
        events.push(ev(
            41,
            2100,
            Payload::ModuleReturned { module_call_id: 1 },
            Source::PythonSidecar,
        ));

        // A third CB with no CbOps (e.g. a blit) in the same window: its
        // overlapped in_flight must not count either.
        events.insert(
            events.len() - 2,
            ev(
                30,
                115,
                Payload::MetalCbCommitted {
                    cb_id: 0xc,
                    queue_id: 1,
                    queue_depth: 3,
                    label: None,
                },
                Source::MetalHook,
            ),
        );
        events.insert(
            events.len() - 2,
            ev(
                31,
                1300,
                Payload::MetalCbCompleted {
                    cb_id: 0xc,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 1185,
                },
                Source::MetalHook,
            ),
        );

        let root = compute(events).unwrap();
        let m = find_child(&root, "m");
        assert_eq!(
            m.gpu_ns_self, 333,
            "window gpu must sum op times (111+222), not in_flight"
        );
        assert_eq!(m.cb_count, 3);
    }

    fn cb_lifecycle(seq: u64, ts: u64, cb_id: u64, op: &str, gpu_ns: u64) -> Vec<Event> {
        vec![
            ev(
                seq,
                ts,
                Payload::MetalCbCommitted {
                    cb_id,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
                Source::MetalHook,
            ),
            ev(
                seq + 1,
                ts + 5,
                Payload::MetalCbCompleted {
                    cb_id,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 5,
                },
                Source::MetalHook,
            ),
            ev(
                seq + 2,
                ts + 6,
                Payload::MetalCbOps {
                    cb_id,
                    ops: vec![op_sample(op, gpu_ns, 1)],
                },
                Source::MetalHook,
            ),
        ]
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
                    stack_frames: vec![],
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
                    fields: Default::default(),
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
                    stack_frames: vec![],
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
                    fields: Default::default(),
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
                    fields: Default::default(),
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
                    stack_frames: vec![],
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
                fields: Default::default(),
            },
            Source::PythonSidecar,
        )];
        let r = compute(evs).unwrap();
        assert_eq!(r.diagnostics.as_ref().unwrap().malformed_returns, 1);
    }

    #[test]
    fn ops_attribution_to_module_leaf() {
        let evs = vec![
            ev(
                1,
                50,
                Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 1,
                    qualname: "Block".into(),
                    class_name: "Block".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields: Default::default(),
                },
                Source::PythonSidecar,
            ),
            ev(
                2,
                60,
                Payload::ModuleEntered {
                    module_call_id: 2,
                    module_def_id: 2,
                    qualname: "Linear".into(),
                    class_name: "Linear".into(),
                    parent_call_id: Some(1),
                    depth: 1,
                    fields: Default::default(),
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
                    stack_frames: vec![],
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
                121,
                Payload::MetalCbOps {
                    cb_id: 9,
                    ops: vec![op_sample("Matmul", 700, 1), op_sample("Softmax", 200, 1)],
                },
                Source::MetalHook,
            ),
            ev(
                7,
                200,
                Payload::MlxEvalReturned {
                    call_id: 7,
                    duration_ns: 100,
                    was_async: false,
                },
                Source::PythonSidecar,
            ),
            ev(
                8,
                210,
                Payload::ModuleReturned { module_call_id: 2 },
                Source::PythonSidecar,
            ),
            ev(
                9,
                220,
                Payload::ModuleReturned { module_call_id: 1 },
                Source::PythonSidecar,
            ),
        ];
        let r = compute(evs).unwrap();
        let block = find_child(&r, "Block");
        let lin = block
            .children
            .iter()
            .find(|c| c.qualname == "Linear")
            .unwrap();
        assert_eq!(lin.ops.len(), 2);
        assert_eq!(lin.ops[0].name, "Matmul");
        assert_eq!(lin.ops[0].gpu_ns, 700);
        assert_eq!(lin.ops[1].name, "Softmax");
        assert!(block.ops.is_empty(), "ops should not propagate to parent");
    }

    #[test]
    fn ops_under_unscoped() {
        let evs = vec![
            ev(
                1,
                100,
                Payload::MlxEvalEntered {
                    call_id: 1,
                    array_count: 1,
                    stream: "gpu".into(),
                    module_stack: vec![],
                    stack_frames: vec![],
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
                121,
                Payload::MetalCbOps {
                    cb_id: 9,
                    ops: vec![op_sample("Matmul", 400, 1), op_sample("Cast", 100, 2)],
                },
                Source::MetalHook,
            ),
            ev(
                5,
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
        let u = find_child(&r, UNSCOPED);
        assert_eq!(u.ops.len(), 2);
        assert_eq!(u.ops[0].name, "Matmul");
    }

    #[test]
    fn ops_merge_dedup_by_name_across_cbs() {
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
                    fields: Default::default(),
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
                    stack_frames: vec![],
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
                    in_flight_ns: 500,
                },
                Source::MetalHook,
            ),
            ev(
                5,
                121,
                Payload::MetalCbOps {
                    cb_id: 9,
                    ops: vec![op_sample("Matmul", 300, 1)],
                },
                Source::MetalHook,
            ),
            ev(
                6,
                130,
                Payload::MetalCbCommitted {
                    cb_id: 10,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
                Source::MetalHook,
            ),
            ev(
                7,
                140,
                Payload::MetalCbCompleted {
                    cb_id: 10,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 200,
                },
                Source::MetalHook,
            ),
            ev(
                8,
                141,
                Payload::MetalCbOps {
                    cb_id: 10,
                    ops: vec![op_sample("Matmul", 300, 2)],
                },
                Source::MetalHook,
            ),
            ev(
                9,
                200,
                Payload::MlxEvalReturned {
                    call_id: 7,
                    duration_ns: 100,
                    was_async: false,
                },
                Source::PythonSidecar,
            ),
            ev(
                10,
                210,
                Payload::ModuleReturned { module_call_id: 1 },
                Source::PythonSidecar,
            ),
        ];
        let r = compute(evs).unwrap();
        let lin = find_child(&r, "Linear");
        assert_eq!(lin.ops.len(), 1);
        assert_eq!(lin.ops[0].gpu_ns, 600);
        assert_eq!(lin.ops[0].count, 3);
    }

    #[test]
    fn cb_without_ops_partial_increments_diagnostic() {
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
                    in_flight_ns: 100,
                },
                Source::MetalHook,
            ),
            ev(
                3,
                111,
                Payload::MetalCbOps {
                    cb_id: 1,
                    ops: vec![op_sample("Matmul", 80, 1)],
                },
                Source::MetalHook,
            ),
            ev(
                4,
                120,
                Payload::MetalCbCommitted {
                    cb_id: 2,
                    queue_id: 1,
                    queue_depth: 1,
                    label: None,
                },
                Source::MetalHook,
            ),
            ev(
                5,
                130,
                Payload::MetalCbCompleted {
                    cb_id: 2,
                    queue_id: 1,
                    status: 4,
                    error_code: None,
                    error_domain: None,
                    in_flight_ns: 50,
                },
                Source::MetalHook,
            ),
            // cb_id 2 has NO MetalCbOps — should be counted
        ];
        let r = compute(evs).unwrap();
        assert_eq!(r.diagnostics.as_ref().unwrap().ops_cbs_without_samples, 1);
    }

    #[test]
    fn cb_without_ops_legacy_mode_silent() {
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
                    in_flight_ns: 100,
                },
                Source::MetalHook,
            ),
        ];
        let r = compute(evs).unwrap();
        assert_eq!(r.diagnostics.as_ref().unwrap().ops_cbs_without_samples, 0);
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
                    fields: Default::default(),
                }],
                ops: vec![],
                diagnostics: None,
                fields: Default::default(),
            }],
            ops: vec![],
            diagnostics: Some(Diagnostics::default()),
            fields: Default::default(),
        }
    }

    fn fixture_with_ops(ops: Vec<OpAttribution>) -> ModuleBreakdown {
        ModuleBreakdown {
            qualname: "<root>".into(),
            class_name: "".into(),
            calls: 0,
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
                ops,
                diagnostics: None,
                fields: Default::default(),
            }],
            ops: vec![],
            diagnostics: Some(Diagnostics::default()),
            fields: Default::default(),
        }
    }

    #[test]
    fn render_table_shows_qualnames_and_durations() {
        let s = render_table(&sample_breakdown(), 10, 6, 5, true);
        assert!(s.contains("Block"));
        assert!(s.contains("Linear"));
        assert!(s.contains("1.500")); // 1500 ns formatted as us
    }

    #[test]
    fn render_table_includes_op_lines_by_default() {
        let r = fixture_with_ops(vec![
            OpAttribution {
                name: "Matmul".into(),
                gpu_ns: 1200,
                count: 1,
                symbol: None,
                kind: None,
            },
            OpAttribution {
                name: "Softmax".into(),
                gpu_ns: 300,
                count: 1,
                symbol: None,
                kind: None,
            },
        ]);
        let s = render_table(&r, 10, 6, 5, true);
        assert!(s.contains("Matmul"));
        assert!(s.contains("Softmax"));
        assert!(s.contains("op:"));
    }

    #[test]
    fn render_table_no_ops_flag_hides_them() {
        let r = fixture_with_ops(vec![OpAttribution {
            name: "Matmul".into(),
            gpu_ns: 1200,
            count: 1,
            symbol: None,
            kind: None,
        }]);
        let s = render_table(&r, 10, 6, 5, false);
        assert!(!s.contains("Matmul"));
    }

    #[test]
    fn render_table_top_ops_caps_at_n() {
        let mut ops = Vec::new();
        for i in 0..10u64 {
            ops.push(OpAttribution {
                name: format!("Op{i}"),
                gpu_ns: 1000 - i * 10,
                count: 1,
                symbol: None,
                kind: None,
            });
        }
        let r = fixture_with_ops(ops);
        let s = render_table(&r, 10, 6, 3, true);
        assert!(s.contains("Op0"));
        assert!(s.contains("Op2"));
        assert!(!s.contains("Op3"));
    }

    #[test]
    fn render_chrome_trace_is_valid_json_with_complete_events() {
        let json = render_chrome_trace(&sample_breakdown());
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = parsed["traceEvents"].as_array().unwrap();
        assert!(arr.iter().any(|e| e["name"] == "Linear" && e["ph"] == "X"));
        assert_eq!(parsed["displayTimeUnit"], "us");
    }

    #[test]
    fn render_chrome_trace_includes_op_events() {
        let r = fixture_with_ops(vec![OpAttribution {
            name: "Matmul".into(),
            gpu_ns: 1000,
            count: 1,
            symbol: None,
            kind: None,
        }]);
        let json = render_chrome_trace(&r);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = parsed["traceEvents"].as_array().unwrap();
        assert!(arr
            .iter()
            .any(|e| e["name"] == "Matmul" && e["cat"] == "op"));
    }

    #[test]
    fn render_ops_flat_groups_by_name() {
        let root = ModuleBreakdown {
            qualname: "<root>".into(),
            class_name: "".into(),
            calls: 0,
            gpu_ns_self: 0,
            gpu_ns_subtree: 3000,
            eval_count: 0,
            cb_count: 0,
            children: vec![
                ModuleBreakdown {
                    qualname: "A".into(),
                    class_name: "A".into(),
                    calls: 1,
                    gpu_ns_self: 1500,
                    gpu_ns_subtree: 1500,
                    eval_count: 1,
                    cb_count: 1,
                    children: vec![],
                    ops: vec![OpAttribution {
                        name: "Matmul".into(),
                        gpu_ns: 1000,
                        count: 1,
                        symbol: None,
                        kind: None,
                    }],
                    diagnostics: None,
                    fields: Default::default(),
                },
                ModuleBreakdown {
                    qualname: "B".into(),
                    class_name: "B".into(),
                    calls: 1,
                    gpu_ns_self: 1500,
                    gpu_ns_subtree: 1500,
                    eval_count: 1,
                    cb_count: 1,
                    children: vec![],
                    ops: vec![
                        OpAttribution {
                            name: "Matmul".into(),
                            gpu_ns: 500,
                            count: 2,
                            symbol: None,
                            kind: None,
                        },
                        OpAttribution {
                            name: "Softmax".into(),
                            gpu_ns: 200,
                            count: 1,
                            symbol: None,
                            kind: None,
                        },
                    ],
                    diagnostics: None,
                    fields: Default::default(),
                },
            ],
            ops: vec![],
            diagnostics: Some(Diagnostics::default()),
            fields: Default::default(),
        };
        let s = render_ops_flat(&root, OpGroupBy::Name, 10);
        assert!(s.contains("Matmul"));
        assert!(s.contains("1.500")); // 1500 ns formatted as us → "1.500"
        assert!(s.contains("Softmax"));
    }

    #[test]
    fn render_ops_flat_groups_by_kind() {
        // Two ops with different kernel names but the same kind "Matmul".
        // In Kind mode they should collapse into a single "Matmul" row and
        // the individual kernel names must not appear in the output.
        let root = ModuleBreakdown {
            qualname: "<root>".into(),
            class_name: "".into(),
            calls: 0,
            gpu_ns_self: 0,
            gpu_ns_subtree: 1500,
            eval_count: 0,
            cb_count: 0,
            children: vec![
                ModuleBreakdown {
                    qualname: "A".into(),
                    class_name: "A".into(),
                    calls: 1,
                    gpu_ns_self: 800,
                    gpu_ns_subtree: 800,
                    eval_count: 1,
                    cb_count: 1,
                    children: vec![],
                    ops: vec![OpAttribution {
                        name: "gemm_kernel_0".into(),
                        gpu_ns: 800,
                        count: 1,
                        symbol: Some("gemm_kernel_0".into()),
                        kind: Some("Matmul".into()),
                    }],
                    diagnostics: None,
                    fields: Default::default(),
                },
                ModuleBreakdown {
                    qualname: "B".into(),
                    class_name: "B".into(),
                    calls: 1,
                    gpu_ns_self: 700,
                    gpu_ns_subtree: 700,
                    eval_count: 1,
                    cb_count: 1,
                    children: vec![],
                    ops: vec![OpAttribution {
                        name: "gemm_kernel_1".into(),
                        gpu_ns: 700,
                        count: 2,
                        symbol: Some("gemm_kernel_1".into()),
                        kind: Some("Matmul".into()),
                    }],
                    diagnostics: None,
                    fields: Default::default(),
                },
            ],
            ops: vec![],
            diagnostics: Some(Diagnostics::default()),
            fields: Default::default(),
        };
        let s = render_ops_flat(&root, OpGroupBy::Kind, 10);
        assert!(s.contains("Matmul"), "Kind row 'Matmul' must appear");
        assert!(
            !s.contains("gemm_kernel_0"),
            "individual kernel name must not appear in Kind mode"
        );
        assert!(
            !s.contains("gemm_kernel_1"),
            "individual kernel name must not appear in Kind mode"
        );
    }

    #[test]
    fn aggregate_ops_flat_by_kind_collapses_kernels() {
        // Build a root with two matmul-kind ops (different names/symbols) + one other.
        let root = ModuleBreakdown {
            qualname: "root".into(),
            class_name: String::new(),
            calls: 0,
            gpu_ns_self: 0,
            gpu_ns_subtree: 0,
            eval_count: 0,
            cb_count: 0,
            ops: vec![
                OpAttribution {
                    name: "gemm_a".into(),
                    gpu_ns: 100,
                    count: 1,
                    symbol: Some("gemm_a".into()),
                    kind: Some("Matmul".into()),
                },
                OpAttribution {
                    name: "gemm_b".into(),
                    gpu_ns: 50,
                    count: 2,
                    symbol: Some("gemm_b".into()),
                    kind: Some("Matmul".into()),
                },
                OpAttribution {
                    name: "K_ff00".into(),
                    gpu_ns: 30,
                    count: 1,
                    symbol: None,
                    kind: None,
                },
            ],
            children: vec![],
            diagnostics: None,
            fields: Default::default(),
        };

        let kind_rows = aggregate_ops_flat(&root, OpGroupBy::Kind);
        let mm = kind_rows
            .iter()
            .find(|r| r.key == "Matmul")
            .expect("Matmul row");
        assert_eq!(mm.gpu_ns, 150);
        assert_eq!(mm.count, 3);
        assert!(mm.symbol.is_none(), "kind mode drops symbol");
        // unresolved kernel falls back to its name as its own row
        assert!(kind_rows.iter().any(|r| r.key == "K_ff00"));

        let by_name = aggregate_ops_flat(&root, OpGroupBy::Name);
        assert!(by_name.iter().any(|r| r.key == "gemm_a"));
        assert!(by_name.iter().any(|r| r.key == "gemm_b"));
        // sorted desc by gpu_ns
        assert!(by_name.windows(2).all(|w| w[0].gpu_ns >= w[1].gpu_ns));
    }

    #[test]
    fn compute_propagates_fields_into_module_breakdown() {
        use smeltr_core::event::{Event, FieldValue, Payload, Source};
        use std::collections::BTreeMap;
        use uuid::Uuid;

        let mut fields = BTreeMap::new();
        fields.insert("layer".into(), FieldValue::Int(3));
        fields.insert("kind".into(), FieldValue::String("matmul".into()));

        let events = vec![
            Event {
                ts_mono_ns: 1_000_000,
                ts_wall_ns: 0,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 1,
                payload: Payload::ModuleEntered {
                    module_call_id: 1,
                    module_def_id: 0,
                    qualname: "forward".into(),
                    class_name: "Scope".into(),
                    parent_call_id: None,
                    depth: 0,
                    fields,
                },
            },
            Event {
                ts_mono_ns: 2_000_000,
                ts_wall_ns: 0,
                session_id: Uuid::nil(),
                source: Source::PythonSidecar,
                pid: None,
                seq: 2,
                payload: Payload::ModuleReturned { module_call_id: 1 },
            },
        ];
        let root = compute(events).unwrap();
        let forward = root
            .children
            .iter()
            .find(|c| c.qualname == "forward")
            .expect("forward child");
        assert_eq!(forward.fields.get("layer"), Some(&FieldValue::Int(3)));
        assert_eq!(
            forward.fields.get("kind"),
            Some(&FieldValue::String("matmul".into()))
        );
    }

    fn leaf_with_ops(qualname: &str, ops: Vec<OpAttribution>) -> ModuleBreakdown {
        ModuleBreakdown {
            qualname: qualname.into(),
            class_name: String::new(),
            calls: 1,
            gpu_ns_self: 0,
            gpu_ns_subtree: 0,
            eval_count: 0,
            cb_count: 0,
            children: vec![],
            ops,
            diagnostics: None,
            fields: Default::default(),
        }
    }

    #[test]
    fn apply_op_group_by_kind_regroups_each_node() {
        // Build a parent with a child; both have matmul kernels + one unresolved.
        let mut root = leaf_with_ops(
            "root",
            vec![
                OpAttribution {
                    name: "gemm_a".into(),
                    gpu_ns: 100,
                    count: 1,
                    symbol: Some("gemm_a".into()),
                    kind: Some("Matmul".into()),
                },
                OpAttribution {
                    name: "gemm_b".into(),
                    gpu_ns: 50,
                    count: 2,
                    symbol: Some("gemm_b".into()),
                    kind: Some("Matmul".into()),
                },
                OpAttribution {
                    name: "K_ff00".into(),
                    gpu_ns: 30,
                    count: 1,
                    symbol: None,
                    kind: None,
                },
            ],
        );
        let child = leaf_with_ops(
            "child",
            vec![OpAttribution {
                name: "gemm_c".into(),
                gpu_ns: 10,
                count: 1,
                symbol: Some("gemm_c".into()),
                kind: Some("Matmul".into()),
            }],
        );
        root.children.push(child);

        apply_op_group_by(&mut root, OpGroupBy::Kind);

        let mm = root
            .ops
            .iter()
            .find(|o| o.name == "Matmul")
            .expect("Matmul row");
        assert_eq!(mm.gpu_ns, 150);
        assert_eq!(mm.count, 3);
        assert!(mm.symbol.is_none());
        assert!(root.ops.iter().any(|o| o.name == "K_ff00")); // fallback row kept
                                                              // descending sort: Matmul (150) before K_ff00 (30)
        assert!(root.ops[0].gpu_ns >= root.ops[root.ops.len() - 1].gpu_ns);
        // child node regrouped too
        assert_eq!(root.children[0].ops.len(), 1);
        assert_eq!(root.children[0].ops[0].name, "Matmul");
    }

    #[test]
    fn apply_op_group_by_name_is_noop() {
        let mut root = leaf_with_ops(
            "root",
            vec![OpAttribution {
                name: "gemm_a".into(),
                gpu_ns: 100,
                count: 1,
                symbol: Some("gemm_a".into()),
                kind: Some("Matmul".into()),
            }],
        );
        let before = root.ops.clone();
        apply_op_group_by(&mut root, OpGroupBy::Name);
        assert_eq!(root.ops, before);
    }

    #[test]
    fn compute_distinguishes_siblings_by_fields() {
        use smeltr_core::event::{Event, FieldValue, Payload, Source};
        use std::collections::BTreeMap;
        use uuid::Uuid;

        let make_pass = |seq: u64, ts: u64, cid: u64, idx: i64| Event {
            ts_mono_ns: ts,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::PythonSidecar,
            pid: None,
            seq,
            payload: Payload::ModuleEntered {
                module_call_id: cid,
                module_def_id: 0,
                qualname: "inner.pass".into(),
                class_name: "Scope".into(),
                parent_call_id: None,
                depth: 0,
                fields: {
                    let mut m = BTreeMap::new();
                    m.insert("pass_idx".into(), FieldValue::Int(idx));
                    m
                },
            },
        };
        let ret = |seq: u64, ts: u64, cid: u64| Event {
            ts_mono_ns: ts,
            ts_wall_ns: 0,
            session_id: Uuid::nil(),
            source: Source::PythonSidecar,
            pid: None,
            seq,
            payload: Payload::ModuleReturned {
                module_call_id: cid,
            },
        };

        let events = vec![
            make_pass(1, 100, 1, 0),
            ret(2, 200, 1),
            make_pass(3, 300, 2, 1),
            ret(4, 400, 2),
            make_pass(5, 500, 3, 2),
            ret(6, 600, 3),
        ];
        let root = compute(events).unwrap();
        let passes: Vec<&ModuleBreakdown> = root
            .children
            .iter()
            .filter(|c| c.qualname == "inner.pass")
            .collect();
        assert_eq!(passes.len(), 3, "three sibling inner.pass nodes");
        let idxs: Vec<&FieldValue> = passes
            .iter()
            .map(|p| p.fields.get("pass_idx").expect("pass_idx present"))
            .collect();
        assert!(idxs.contains(&&FieldValue::Int(0)));
        assert!(idxs.contains(&&FieldValue::Int(1)));
        assert!(idxs.contains(&&FieldValue::Int(2)));
    }
}

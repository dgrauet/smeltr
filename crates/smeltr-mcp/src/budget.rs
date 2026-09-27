//! Response-size guard: no tool result goes out larger than a budget.
//!
//! Per-tool defaults and caps keep ordinary calls small, but some shapes
//! still grow with the session (a breakdown tree, dispatch origins, a
//! memory timeline). Past the budget the result is cut — largest array
//! first, from its end — and a `_truncated` object says what was cut, from
//! how much, and how to narrow the call. Never a silent cut (#271).

use serde_json::{json, Map, Value};

/// Upper bound on a serialized tool result, in bytes of compact JSON.
///
/// Claude Code refuses tool output above 25 000 tokens by default
/// (`MAX_MCP_OUTPUT_TOKENS`). JSON full of numbers and identifiers runs
/// at roughly 3 bytes per token, so 60 000 bytes is ~20 000 tokens: under
/// that limit with room for the rest of the turn.
pub const RESPONSE_BUDGET_CHARS: usize = 60_000;

/// Key of the notice added to a cut result.
pub const NOTICE_KEY: &str = "_truncated";

/// Strings shorter than this are never cut: they cannot free much, and cutting
/// a name or an id would only make the result misleading.
const MIN_CUT_STRING: usize = 64;

/// `value` unchanged when its compact JSON fits `budget` bytes; otherwise
/// cut to fit, with a [`NOTICE_KEY`] object naming each cut and `hint`.
/// A top-level non-object is wrapped as `{"result": …}`.
pub fn fit(value: Value, budget: usize, hint: &str) -> Value {
    let original = size_of(&value);
    if original <= budget {
        return value;
    }
    let mut root = match value {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("result".into(), other);
            m
        }
    };
    let mut cuts: Vec<Cut> = Vec::new();
    // Every pass frees at least one element, key or string byte.
    loop {
        root.insert(
            NOTICE_KEY.into(),
            notice(budget, original, hint, &cuts, false),
        );
        let mut best = Best::default();
        let size = walk_root(&root, &mut best);
        root.remove(NOTICE_KEY);
        if size <= budget {
            break;
        }
        let excess = size - budget;
        let Some(target) = best.pick() else {
            // Nothing left that can be cut: say so rather than overflow.
            let mut m = Map::new();
            m.insert(
                NOTICE_KEY.into(),
                notice(budget, original, hint, &cuts, true),
            );
            return Value::Object(m);
        };
        let Some(node) = pointer_mut(&mut root, &target.path) else {
            break;
        };
        let (kept, of, unit) = cut(node, excess);
        let path = pointer(&target.path);
        match cuts.iter_mut().find(|c| c.path == path) {
            Some(c) => c.kept = kept,
            None => cuts.push(Cut {
                path,
                kept,
                of,
                unit,
            }),
        }
    }
    root.insert(
        NOTICE_KEY.into(),
        notice(budget, original, hint, &cuts, false),
    );
    Value::Object(root)
}

struct Cut {
    path: String,
    kept: usize,
    of: usize,
    unit: &'static str,
}

fn notice(budget: usize, original: usize, hint: &str, cuts: &[Cut], all: bool) -> Value {
    let cut: Vec<Value> = cuts
        .iter()
        .map(|c| json!({"path": c.path, "kept": c.kept, "of": c.of, "unit": c.unit}))
        .collect();
    let mut n = json!({
        "budget_chars": budget,
        "original_chars": original,
        "cut": cut,
        "hint": hint,
    });
    if all {
        n["dropped"] = json!("the whole result: nothing in it could be cut to fit");
    }
    n
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// An array or a long string.
    Sequence,
    /// A non-root object: its keys are cut only when no sequence is left.
    Object,
}

struct Candidate {
    score: usize,
    path: Vec<String>,
}

#[derive(Default)]
struct Best {
    sequence: Option<Candidate>,
    object: Option<Candidate>,
}

impl Best {
    fn offer(&mut self, kind: Kind, score: usize, path: &[String]) {
        let slot = match kind {
            Kind::Sequence => &mut self.sequence,
            Kind::Object => &mut self.object,
        };
        if slot.as_ref().is_none_or(|c| score > c.score) {
            *slot = Some(Candidate {
                score,
                path: path.to_vec(),
            });
        }
    }

    fn pick(self) -> Option<Candidate> {
        self.sequence.or(self.object)
    }
}

/// Size of `root` as compact JSON, collecting cut candidates below it
/// (never the root itself, never the notice).
fn walk_root(root: &Map<String, Value>, best: &mut Best) -> usize {
    let mut path = Vec::new();
    let mut size = 2 + root.len().saturating_sub(1);
    for (k, v) in root {
        size += str_size(k) + 1;
        if k == NOTICE_KEY {
            size += size_of(v);
            continue;
        }
        path.push(k.clone());
        size += walk(v, &mut path, best);
        path.pop();
    }
    size
}

fn walk(v: &Value, path: &mut Vec<String>, best: &mut Best) -> usize {
    match v {
        // A container is ranked by what it holds besides its largest member:
        // cutting `[{"children": [...]}]` would drop everything at once,
        // where the array inside can be cut item by item.
        Value::Array(a) => {
            let mut size = 2 + a.len().saturating_sub(1);
            let mut largest = 0;
            for (i, e) in a.iter().enumerate() {
                path.push(i.to_string());
                let s = walk(e, path, best);
                path.pop();
                size += s;
                largest = largest.max(s);
            }
            if !a.is_empty() {
                best.offer(Kind::Sequence, size - largest, path);
            }
            size
        }
        Value::Object(m) => {
            let mut size = 2 + m.len().saturating_sub(1);
            let mut largest = 0;
            for (k, e) in m {
                path.push(k.clone());
                let s = str_size(k) + 1 + walk(e, path, best);
                path.pop();
                size += s;
                largest = largest.max(s);
            }
            if !m.is_empty() {
                best.offer(Kind::Object, size - largest, path);
            }
            size
        }
        Value::String(s) => {
            let size = str_size(s);
            if s.len() >= MIN_CUT_STRING {
                best.offer(Kind::Sequence, size, path);
            }
            size
        }
        other => size_of(other),
    }
}

fn size_of(v: &Value) -> usize {
    serde_json::to_string(v).map(|s| s.len()).unwrap_or(0)
}

fn str_size(s: &str) -> usize {
    serde_json::to_string(s).map(|s| s.len()).unwrap_or(0)
}

/// Frees at least `excess` bytes from `node` (or all of it); returns
/// (kept, original count, unit).
fn cut(node: &mut Value, excess: usize) -> (usize, usize, &'static str) {
    match node {
        Value::Array(a) => {
            let of = a.len();
            let mut freed = 0;
            while freed < excess {
                let Some(last) = a.pop() else { break };
                // The element, and the comma before it if any remain.
                freed += size_of(&last) + usize::from(!a.is_empty());
            }
            (a.len(), of, "items")
        }
        Value::String(s) => {
            let of = s.chars().count();
            // Each raw byte removed frees at least one byte of JSON.
            let mut end = s.len().saturating_sub(excess);
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            s.truncate(end);
            (s.chars().count(), of, "chars")
        }
        Value::Object(m) => {
            let of = m.len();
            let mut freed = 0;
            let mut drop = Vec::new();
            for (k, v) in m.iter().rev() {
                if freed >= excess {
                    break;
                }
                freed += str_size(k) + 1 + size_of(v) + 1;
                drop.push(k.clone());
            }
            for k in drop {
                m.remove(&k);
            }
            (m.len(), of, "keys")
        }
        _ => (0, 0, "items"),
    }
}

fn pointer_mut<'a>(root: &'a mut Map<String, Value>, path: &[String]) -> Option<&'a mut Value> {
    let (first, rest) = path.split_first()?;
    let mut node = root.get_mut(first)?;
    for seg in rest {
        node = match node {
            Value::Object(m) => m.get_mut(seg)?,
            Value::Array(a) => a.get_mut(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(node)
}

/// RFC 6901 JSON Pointer.
fn pointer(path: &[String]) -> String {
    path.iter()
        .map(|s| format!("/{}", s.replace('~', "~0").replace('/', "~1")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn len(v: &Value) -> usize {
        serde_json::to_string(v).unwrap().len()
    }

    fn events(n: usize) -> Vec<Value> {
        (0..n)
            .map(|i| json!({"seq": i, "payload": {"kind": "Mark", "label": format!("event-{i}")}}))
            .collect()
    }

    #[test]
    fn a_result_within_budget_is_untouched() {
        let v = json!({"events": events(10), "matched": 10});
        assert_eq!(fit(v.clone(), 10_000, "narrow it"), v);
    }

    /// #271: a result over budget is cut, and says so: where, how much,
    /// and how to get the rest. Never a silent cut.
    #[test]
    fn the_largest_array_is_cut_from_its_end_and_the_cut_is_reported() {
        let v = json!({"events": events(1000), "matched": 1000, "tags": [1, 2, 3]});
        let original = len(&v);
        let out = fit(v, 5_000, "lower `limit`");
        assert!(len(&out) <= 5_000, "{}", len(&out));
        let kept = out["events"].as_array().unwrap();
        assert!(!kept.is_empty() && kept.len() < 1000, "{}", kept.len());
        for (i, e) in kept.iter().enumerate() {
            assert_eq!(e["seq"], i, "the first events are kept, in order");
        }
        assert_eq!(out["matched"], 1000);
        assert_eq!(out["tags"], json!([1, 2, 3]), "small arrays untouched");
        let t = &out["_truncated"];
        assert_eq!(t["budget_chars"], 5_000);
        assert_eq!(t["original_chars"], original);
        assert_eq!(t["hint"], "lower `limit`");
        assert_eq!(t["cut"][0]["path"], "/events");
        assert_eq!(t["cut"][0]["kept"], kept.len());
        assert_eq!(t["cut"][0]["of"], 1000);
    }

    #[test]
    fn nested_arrays_are_cut_where_they_are() {
        let v = json!({"root": {"children": [{"children": events(500)}]}});
        let out = fit(v, 4_000, "h");
        assert!(len(&out) <= 4_000);
        assert_eq!(
            out["_truncated"]["cut"][0]["path"],
            "/root/children/0/children"
        );
    }

    /// A single huge string (a crash report) is cut on a char boundary.
    #[test]
    fn a_long_string_is_cut_on_a_char_boundary() {
        let text: String = "é".repeat(10_000);
        let out = fit(json!({"text": text}), 3_000, "page with offset");
        assert!(len(&out) <= 3_000);
        let kept = out["text"].as_str().unwrap();
        assert!(!kept.is_empty() && kept.chars().all(|c| c == 'é'));
        assert_eq!(out["_truncated"]["cut"][0]["path"], "/text");
        assert_eq!(out["_truncated"]["cut"][0]["unit"], "chars");
        assert_eq!(out["_truncated"]["cut"][0]["of"], 10_000);
    }

    #[test]
    fn a_top_level_array_is_wrapped() {
        let out = fit(Value::Array(events(1000)), 3_000, "h");
        assert!(len(&out) <= 3_000);
        assert!(out["result"].is_array());
        assert!(out["_truncated"].is_object());
    }

    #[test]
    fn keys_are_escaped_in_the_pointer() {
        let v = json!({"a/b": {"c~d": events(500)}});
        let out = fit(v, 3_000, "h");
        assert_eq!(out["_truncated"]["cut"][0]["path"], "/a~1b/c~0d");
    }

    /// Whatever the shape, the result fits.
    #[test]
    fn always_fits() {
        let shapes = [
            json!({"a": events(300), "b": events(300), "c": "x".repeat(5_000)}),
            json!({"m": (0..2_000).map(|i| (format!("k{i}"), json!(i))).collect::<serde_json::Map<_, _>>()}),
            json!({"deep": [[[[events(400)]]]]}),
        ];
        for v in shapes {
            for budget in [1_500, 4_000, 20_000] {
                let out = fit(v.clone(), budget, "h");
                assert!(len(&out) <= budget, "budget {budget}: {}", len(&out));
                assert!(out["_truncated"].is_object());
            }
        }
    }
}

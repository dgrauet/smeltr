#!/usr/bin/env bash
#
# One-shot end-to-end smoke test for a local smeltr install on macOS
# Apple Silicon. Records the canonical workload (`smoke_workload.py`),
# exports the session to JSON + chrome-trace, and asserts on the JSON
# (more robust than grep on Debug formatting).
#
# Usage:
#   scripts/dogfood/smoke-test.sh            # uses ./target/release/smeltr
#   SMELTR=/path/to/smeltr scripts/dogfood/smoke-test.sh
#   PYTHON=/path/to/python scripts/dogfood/smoke-test.sh
#
# Requires: python3 (system), jq optional. Exit 0 if all PASS, 1 otherwise.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

SMELTR="${SMELTR:-$REPO_ROOT/target/release/smeltr}"
PYTHON="${PYTHON:-$REPO_ROOT/python/.venv/bin/python}"
WORKLOAD="$REPO_ROOT/scripts/dogfood/smoke_workload.py"
SESSION_NAME="smoke-$(date +%s)"

PASS=0
FAIL=0

step() { printf '[%-30s] %s\n' "$1" "$2"; }

# `assert_py KEY 'python expression that should print "PASS" or "FAIL"'`
assert_py() {
    local label="$1" code="$2"
    local result
    result="$(python3 -c "$code" 2>&1 || echo "FAIL: $?")"
    case "$result" in
        PASS) step "$label" "PASS"; PASS=$((PASS + 1)) ;;
        *)    step "$label" "FAIL — $result"; FAIL=$((FAIL + 1)) ;;
    esac
}

# --- pre-flight ----------------------------------------------------------

[[ -x "$SMELTR" ]] || { echo "smeltr binary not found at $SMELTR"; exit 1; }
[[ -x "$PYTHON" ]] || { echo "python not found at $PYTHON"; exit 1; }
[[ -f "$WORKLOAD" ]] || { echo "workload not found at $WORKLOAD"; exit 1; }

VERSION="$("$SMELTR" --version 2>/dev/null | awk '{print $2}')"
step "smeltr version" "$VERSION"

if ! "$SMELTR" daemon status >/dev/null 2>&1; then
    "$SMELTR" daemon start >/dev/null
    sleep 1
fi
DAEMON_PID="$("$SMELTR" daemon status 2>&1 | awk '/pid:/ {print $2}')"
step "daemon pid" "$DAEMON_PID"

# --- record canonical workload -------------------------------------------

RECORD_OUT="$(mktemp)"
SMELTR_STACK_CAPTURE=1 "$SMELTR" record --name "$SESSION_NAME" -- "$PYTHON" "$WORKLOAD" >"$RECORD_OUT" 2>&1
if ! grep -q "smoke_workload: OK" "$RECORD_OUT"; then
    step "record" "FAIL — output at $RECORD_OUT"
    exit 1
fi
step "record" "OK (session=$SESSION_NAME)"

# Resolve short_id by name via `sessions ls`. Session dir name is
# `YYYY-MM-DD-HHMMSS-<8hex>`; short_id is the last hyphen-separated
# component of the first column.
SHORT_ID="$("$SMELTR" sessions ls 2>/dev/null \
    | grep "name=\"$SESSION_NAME\"" \
    | tail -1 \
    | awk '{print $1}' \
    | awk -F- '{print $NF}')"
[[ -n "$SHORT_ID" ]] || { step "resolve short_id" "FAIL"; exit 1; }
step "resolve short_id" "$SHORT_ID"

# Use working-dir paths for exports so debugging is easier; macOS `mktemp -t`
# returns a path without the dot-extension we want and the subsequent
# concatenation breaks the actual file location.
EXPORT_JSON="$(pwd)/.smoke-test-export.json"
EXPORT_TRACE="$(pwd)/.smoke-test-trace.json"
rm -f "$EXPORT_JSON" "$EXPORT_TRACE"
"$SMELTR" export "$SHORT_ID" --format json --output "$EXPORT_JSON" >/dev/null 2>&1
"$SMELTR" export "$SHORT_ID" --format chrome-trace --output "$EXPORT_TRACE" >/dev/null 2>&1
[[ -f "$EXPORT_JSON" && -f "$EXPORT_TRACE" ]] || {
    step "export artifacts" "FAIL — missing $EXPORT_JSON or $EXPORT_TRACE"
    exit 1
}

# --- per-gap assertions via JSON parsing ---------------------------------

JSON="$EXPORT_JSON"
TRACE="$EXPORT_TRACE"

# #31 Gap 1 — scope events in this session.
assert_py "gap1 scope-token routing" "
import json
with open('$JSON') as f:
    d = json.load(f)
events = d if isinstance(d, list) else d.get('events', d)
mods = [e for e in events if e.get('payload', {}).get('kind') == 'ModuleEntered']
names = {e['payload']['qualname'] for e in mods}
print('PASS' if 'denoise.step' in names and 'outer.loop' in names else 'FAIL: ' + str(names))
"

# #31 Gap 3 — session resolvable by name through MCP-style resolver.
assert_py "gap3 name in sessions ls" "
import subprocess
r = subprocess.run(['$SMELTR', 'sessions', 'ls'], capture_output=True, text=True)
print('PASS' if 'name=\"$SESSION_NAME\"' in r.stdout else 'FAIL: not in ls output')
"

# #38 / #31 Gap 2 — MlxEvalEntered events carry non-empty stack_frames.
assert_py "gap2 stack_frames non-empty" "
import json
with open('$JSON') as f:
    d = json.load(f)
events = d if isinstance(d, list) else d.get('events', d)
evals = [e for e in events if e.get('payload', {}).get('kind') == 'MlxEvalEntered']
non_empty = sum(1 for e in evals if e['payload'].get('stack_frames'))
print('PASS' if non_empty > 0 else f'FAIL: 0/{len(evals)} evals have stack_frames')
"

# #38 — dispatch_origins non-empty.
assert_py "gap38 dispatch_origins" "
import subprocess
r = subprocess.run(['$SMELTR', 'origins', '$SHORT_ID'], capture_output=True, text=True)
print('PASS' if 'smoke_workload.py' in r.stdout else 'FAIL: ' + r.stdout[:120])
"

# #40 / #47 — every user scope has SAMPLES > 0 in the scope-memory table.
# Output format: `<qualname>  <peak>  <avg>  <end>  <samples>` where each
# column is whitespace-aligned. SAMPLES is the last numeric column.
assert_py "gap40+47 mem samples all scopes" "
import subprocess, re
r = subprocess.run(['$SMELTR', 'memory', '$SHORT_ID'], capture_output=True, text=True)
# Take only the SCOPE PEAK MEMORY section (lines after the header until blank).
section = []
in_section = False
for line in r.stdout.splitlines():
    if line.startswith('SCOPE PEAK MEMORY'):
        in_section = True
        continue
    if in_section:
        if not line.strip():
            break
        section.append(line)
scopes = ['denoise.step', 'outer.loop', 'inner.pass', 'typed']
fails = []
for scope in scopes:
    matching = [l for l in section if l.lstrip().startswith(scope)]
    if not matching:
        fails.append(f'{scope}: no row')
        continue
    # SAMPLES is the last whitespace-separated integer.
    samples = max(int(re.findall(r'\d+', l)[-1]) for l in matching if re.findall(r'\d+', l))
    if samples == 0:
        fails.append(f'{scope}: samples=0')
print('PASS' if not fails else 'FAIL: ' + '; '.join(fails))
"

# #43 — scope fields persisted in ModuleEntered.fields.
assert_py "gap43 scope fields persisted" "
import json
with open('$JSON') as f:
    d = json.load(f)
events = d if isinstance(d, list) else d.get('events', d)
denoise = [e for e in events if e.get('payload', {}).get('kind') == 'ModuleEntered'
           and e['payload'].get('qualname') == 'denoise.step']
ok = (
    denoise
    and denoise[0]['payload'].get('fields', {}).get('step') == 5
    and denoise[0]['payload'].get('fields', {}).get('sigma') == 0.5
)
print('PASS' if ok else 'FAIL: ' + str(denoise[:1]))
"

# #46 — fields surfaced in JSON export (same as gap43 but explicit name).
assert_py "gap46 fields in json export" "
import json
with open('$JSON') as f:
    d = json.load(f)
events = d if isinstance(d, list) else d.get('events', d)
scoped_fields = [e['payload'].get('fields') for e in events
                 if e.get('payload', {}).get('kind') == 'ModuleEntered']
non_empty = [f for f in scoped_fields if f]
print('PASS' if non_empty else 'FAIL: all fields maps empty')
"

# #46 — fields merged into chrome-trace args.
assert_py "gap46 fields in trace args" "
import json
with open('$TRACE') as f:
    d = json.load(f)
events = d if isinstance(d, list) else d.get('traceEvents', d)
denoise = [e for e in events if e.get('name') == 'denoise.step']
ok = denoise and denoise[0].get('args', {}).get('step') == 5
print('PASS' if ok else 'FAIL: ' + str(denoise[:1]))
"

# v0.4.2 — mark with structured fields (label is clean, fields present).
assert_py "v0.4.2 mark structured fields" "
import json
with open('$JSON') as f:
    d = json.load(f)
events = d if isinstance(d, list) else d.get('events', d)
marks = [e for e in events if e.get('payload', {}).get('kind') == 'Mark']
m = marks[0] if marks else {}
p = m.get('payload', {})
clean_label = p.get('label') == 'smoke-checkpoint'
fields_present = p.get('fields', {}).get('phase') == 'final' and p.get('fields', {}).get('ok') is True
print('PASS' if clean_label and fields_present else f'FAIL: {p}')
"

# v0.4.1 — synchronous scope_enter/scope_exit samples.
assert_py "v0.4.1 scope enter/exit samples" "
import json
with open('$JSON') as f:
    d = json.load(f)
events = d if isinstance(d, list) else d.get('events', d)
samples = [e['payload'] for e in events
           if e.get('payload', {}).get('kind') == 'MetalDeviceMemSample']
at = {s.get('at_event') for s in samples}
print('PASS' if 'scope_enter' in at and 'scope_exit' in at else 'FAIL: ' + str(at))
"

# PR #50 — field-filter integration.
assert_py "PR50 field-filter pass_idx=1" "
import subprocess
r = subprocess.run(['$SMELTR', 'breakdown', '$SHORT_ID', '--field', 'pass_idx=1'],
                   capture_output=True, text=True)
# Filter should leave at most one inner.pass row (the matching one),
# whereas without filter the breakdown has 3 inner.pass rows.
inner = sum(1 for l in r.stdout.splitlines() if 'inner.pass' in l)
print('PASS' if inner == 1 else f'FAIL: {inner} inner.pass rows (expected 1)')
"

# v0.6.0 PR1 — ModelLoad events emitted (sidecar wrapped mx.load).
assert_py "v0.6.0 model_load events present" "
import json
with open('$JSON') as f:
    d = json.load(f)
events = d if isinstance(d, list) else d.get('events', d)
loads = [e for e in events if e.get('payload', {}).get('kind') == 'ModelLoad']
paths = [e['payload'].get('path', '') for e in loads]
ok = sum(1 for p in paths if p.endswith('smoke-model.safetensors')) >= 2
print('PASS' if ok else f'FAIL: {len(loads)} ModelLoad events, paths={paths}')
"

# v0.6.0 PR1 — duplicate-model-load analyzer rule fires.
assert_py "v0.6.0 duplicate-model-load finding" "
import subprocess
r = subprocess.run(['$SMELTR', 'analyze', '$SHORT_ID'], capture_output=True, text=True)
out = r.stdout.lower()
print('PASS' if 'duplicate load of' in out or 'loaded 2 times' in out else 'FAIL: ' + r.stdout[:200])
"

# v0.6.0 PR3 — chrome-trace exposes Model Loads swim lane + counter track.
assert_py "v0.6.0 chrome-trace model lane + counter" "
import json
with open('$TRACE') as f:
    d = json.load(f)
events = d if isinstance(d, list) else d.get('traceEvents', d)
lane = [e for e in events if e.get('pid') == 4 and e.get('ph') == 'X' and e.get('cat') == 'model-load']
counters = [e for e in events if e.get('ph') == 'C' and str(e.get('name','')).startswith('model:')]
ok = len(lane) >= 2 and len(counters) >= 2
print('PASS' if ok else f'FAIL: {len(lane)} lane events, {len(counters)} counters')
"

# v0.6.x ModelUnload events emitted.
assert_py "v0.6.x model unload events emitted" "
import json
with open('$JSON') as f:
    d = json.load(f)
events = d if isinstance(d, list) else d.get('events', d)
unloads = [e for e in events if e.get('payload', {}).get('kind') == 'ModelUnload']
ok = len(unloads) >= 1
print('PASS' if ok else f'FAIL: {len(unloads)} ModelUnload events (expected >=1)')
"

# v0.6.x duplicate-model-load count is exactly 1 (not 2 or 3).
# load #1 -> load #2 (dup) -> unload -> load #3 (not dup) = exactly 1 duplicate.
assert_py "v0.6.x duplicate count is exactly 1" "
import subprocess
r = subprocess.run(['$SMELTR', 'analyze', '$SHORT_ID'], capture_output=True, text=True)
out = r.stdout.lower()
# Count lines containing 'duplicate load of' (one per duplicate finding).
dup_lines = [l for l in out.splitlines() if 'duplicate load of' in l]
print('PASS' if len(dup_lines) == 1 else f'FAIL: {len(dup_lines)} duplicate-load lines (expected 1); output={r.stdout[:200]}')
"

# --- summary -------------------------------------------------------------

echo
echo "----------------------------------------------------------------"
echo "smoke-test: $PASS pass, $FAIL fail"
echo "session:    $SESSION_NAME (short=$SHORT_ID)"
echo "artifacts:"
echo "  record:      $RECORD_OUT"
echo "  json:        $EXPORT_JSON"
echo "  chrome-trace: $EXPORT_TRACE"
echo "----------------------------------------------------------------"

[[ $FAIL -eq 0 ]] && exit 0 || exit 1

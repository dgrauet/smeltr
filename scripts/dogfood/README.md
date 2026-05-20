# Dogfood scripts

End-to-end and stress workloads used to validate a local smeltr install.

## `smoke-test.sh`

One-shot end-to-end verification on macOS Apple Silicon. Records the
canonical workload (`smoke_workload.py`), exports the session to JSON +
chrome-trace, and asserts on every surface previously regressed in
issues #19, #31, #38, #40, #43, #46, #47.

```bash
scripts/dogfood/smoke-test.sh
# 11 PASS / 0 FAIL ⇒ exit 0
```

Override binary / interpreter:

```bash
SMELTR=/opt/homebrew/bin/smeltr PYTHON=$(which python3) scripts/dogfood/smoke-test.sh
```

The script exits non-zero on the first hard failure (record fails,
short_id can't be resolved, export artifacts missing) or if any of the
per-gap assertions fail. Inspect the printed artifact paths to debug.

### What it checks

| Assertion | Validates |
|---|---|
| `gap1 scope-token routing` | scope events land in the recorded session (not ambient) |
| `gap3 name in sessions ls` | `--name X` persists and is visible in `sessions ls` |
| `gap2 stack_frames non-empty` | `SMELTR_STACK_CAPTURE=1` populates `MlxEvalEntered.stack_frames` |
| `gap38 dispatch_origins` | `smeltr origins` returns non-empty results (async-grace fix) |
| `gap40+47 mem samples all scopes` | every user scope has `samples > 0`, including pass-through scopes |
| `gap43 scope fields persisted` | `smeltr.scope(name, **fields)` stores typed values in `ModuleEntered.fields` |
| `gap46 fields in json export` | export surfaces `fields` map |
| `gap46 fields in trace args` | chrome-trace merges `fields` into `args` |
| `v0.4.2 mark structured fields` | `smeltr.mark(label, **fields)` stores structured (no JSON-in-label) |
| `v0.4.1 scope enter/exit samples` | synchronous MTL reads bracket every user scope |
| `PR50 field-filter pass_idx=1` | `smeltr breakdown --field key=value` filters the tree |

### Requirements

- `target/release/smeltr` + `target/release/smeltrd` built (or override
  via `SMELTR=`).
- `python/.venv/bin/python` with smeltr sidecar editable-installed.
- `mlx` available in that venv (the workload uses it).
- `python3` on PATH (system Python is fine — used only for JSON parsing).

## `smoke_workload.py`

The Python workload `smoke-test.sh` records. Standalone-runnable for
ad-hoc record sessions:

```bash
SMELTR_STACK_CAPTURE=1 smeltr record --name foo -- python smoke_workload.py
```

## `mlx_no_crash.py` / `mlx_stress.py`

Older v0.1.0-era workloads kept for historical comparison and
crash-trigger testing.

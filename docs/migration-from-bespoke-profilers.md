# Migrating from a bespoke MLX profiler to smeltr

A practical guide for MLX projects (LTX-2, MLX-LM, etc.) that ship their
own in-tree profiler — typically a context manager that times code
blocks via `time.perf_counter()` and dumps a JSON report at the end.
This guide shows how to swap that for smeltr-native instrumentation.

## Why migrate?

The bespoke pattern works but plateaus quickly:

- **Time only.** A typical `profiling.section(...)` measures wall-clock
  duration but knows nothing about GPU command-buffer lifecycle, op
  attribution, memory allocation, or stack origins.
- **Synchronous semantics.** MLX is lazy. `t = time.perf_counter()` ...
  `mx.eval(x)` ... `t2 = time.perf_counter()` measures the eval
  function call, not the work that completes asynchronously on the GPU.
  smeltr's analyzer applies a 500 ms async-grace window so post-eval
  CB completions still attribute correctly.
- **No correlation.** Bespoke reports show "denoise.step: 7.4 s" but
  not "of which 4.3 s in Matmul kernels dispatched from
  attention.py:127". smeltr's `get_inference_breakdown` + symbolic
  kernel names + dispatch origins close that gap.

## End-state: API equivalence

Most bespoke profilers expose a context manager and a `mark` function.
Both translate cleanly to smeltr.

### Bespoke `section(name, **fields)` ↔ `smeltr.scope(name, **fields)`

Before:

```python
from ltx2_profile import section

with section("denoise.step", step=step_idx, sigma=float(sigma), synchronize=True):
    cond_x0 = model(**cond_kwargs)
    mx.eval(cond_x0)
```

After:

```python
import smeltr

with smeltr.scope("denoise.step", step=step_idx, sigma=float(sigma)):
    cond_x0 = model(**cond_kwargs)
    mx.eval(cond_x0)
```

- `synchronize=True` semantics: not needed. smeltr's async-grace
  window keeps post-eval CBs inside the scope's attribution.
- `**fields` are typed (`bool` / `int` / `float` / `str`) and round-trip
  through the wire to the analyzer; non-primitives are stringified.

### Bespoke `event(label, **fields)` ↔ `smeltr.mark(label, **fields)`

```python
# Before
profiling.event("checkpoint", step=5, phase="warmup", ok=True)

# After
smeltr.mark("checkpoint", step=5, phase="warmup", ok=True)
```

Same shape; smeltr persists `label` clean and `fields` typed (v0.4.2+).

### Decorator form

```python
@smeltr.scope("forward", layer=3)
def forward(self, x): ...
```

(Bespoke profilers usually offer this too — drop-in.)

## Boot sequence

The bespoke profiler typically requires an `init()` call. smeltr has
**three** options, in order of preference:

1. **`smeltr record`** wraps any command:

   ```bash
   smeltr record --name "ltx2-baseline" -- python pipeline.py
   ```

   No code change — the wrapper sets `SMELTR_AUTOLOAD=1` and the
   sidecar attaches automatically via a site-packages `.pth`.

2. **Explicit `smeltr.attach()`** for code that runs outside `smeltr
   record` (notebooks, pytest):

   ```python
   import smeltr
   smeltr.attach()
   # ... your code ...
   smeltr.detach()  # optional; atexit also flushes
   ```

3. **`smeltrd` always-on** (LaunchAgent) catches every smeltr-attached
   process. Set up once:

   ```bash
   smeltr daemon install     # one-time
   smeltr daemon status      # confirm running
   ```

## Replacing the JSON report

The bespoke `dump_report()` returning a dict per scope: replace by one
of three smeltr views.

### CLI: `smeltr breakdown <session-ref>`

```bash
smeltr breakdown ltx2-baseline --top 10
```

Tree view: scope → child scopes → top kernels. Same hierarchy your
bespoke report had, but with real GPU time and per-op breakdown.

Filter by field (v0.4.2+):

```bash
smeltr breakdown ltx2-baseline --field step=5 --field kind=matmul
```

### MCP from an LLM client (Claude Code / Claude Desktop)

If you've added smeltr to your MCP config, ask in plain English:

> "Compare baseline and batched-cfg, show the scope deltas."
>
> → Claude calls `compare_sessions` and `get_inference_breakdown`.

### Programmatic from Python

```python
result = smeltr.export("trace.json", format="chrome-trace")
# Open in chrome://tracing or Perfetto.
```

Or invoke the CLI from a script:

```python
import subprocess
subprocess.run(["smeltr", "export", "ltx2-baseline",
                "--format", "json", "--output", "report.json"])
```

The JSON has the same shape your bespoke `dump_report()` produced:
nested scopes, fields, child counters.

## What the bespoke profiler did that smeltr does *better*

| Bespoke feature | smeltr equivalent | Notes |
|---|---|---|
| `section("foo", **fields)` | `smeltr.scope("foo", **fields)` | typed fields, surfaced in breakdown + chrome-trace |
| `event("checkpoint", **fields)` | `smeltr.mark("checkpoint", **fields)` | structured payload |
| Per-section wall time | `gpu_ns_self` / `gpu_ns_subtree` in breakdown | real GPU time, not eval-return time |
| Per-section JSON dump | `smeltr export --format json` | adds wire schema, kernel symbols, op kinds |
| `synchronize=True` flag | not needed | 500 ms async-grace in analyzer |
| Custom metadata in name | structured `fields=` kwarg | siblings stay distinct; analyzer dedupes by qualname |

## What the bespoke profiler did that smeltr does *additionally*

- **Symbolic kernel names** (e.g. `gemm_t_n_bf16_64_64_32`) and a
  canonical `kind` (`Matmul`, `ScaledDotProductAttention`, …).
- **Per-CB op breakdown** with per-op GPU time.
- **Dispatch origins**: `SMELTR_STACK_CAPTURE=1` ties each kernel back
  to the Python source `attention.py:127`.
- **Per-scope memory peak / avg / end** + heap allocation tracking
  (helps debug watchdog OOMs).
- **Chrome-trace export** opens in Perfetto / Speedscope.
- **Session diffing**: `smeltr compare A B` → scope deltas, op-kind
  deltas, memory deltas, origin deltas in one report.

## What you lose

- **In-process Python aggregation.** Bespoke profilers can compute a
  `dict[scope, dt]` inline. smeltr persists events to disk and reads
  back. For workloads that need real-time numbers in Python:

  ```python
  import smeltr
  # smeltr.scope(...) blocks emit events; query via subprocess or MCP
  # at end of run.
  ```

- **Custom report formats.** smeltr exports JSON + chrome-trace
  natively. For HTML / Markdown reports, post-process the JSON.

## Removal checklist

1. `git grep -l "from ltx2_profile\|from .profile import"` — find imports.
2. Replace `section(name, **fields)` → `smeltr.scope(name, **fields)`.
3. Replace `event(label, **fields)` → `smeltr.mark(label, **fields)`.
4. Drop `synchronize=True` if present (no-op for smeltr).
5. Delete `LTX2_PROFILE` (or equivalent) env-var checks; smeltr is
   either attached (no-op when not) or active via `smeltr record`.
6. Remove the bespoke profiler module + its `dump_report()` call sites.
7. Add an `atexit` hook OR `--export-on-exit` flag if you want a JSON
   dump at end of run:

   ```python
   import atexit, smeltr
   atexit.register(lambda: smeltr.export("trace.json", format="chrome-trace"))
   ```

## Verify the migration

Run the canonical smoke test on your hardware:

```bash
scripts/dogfood/smoke-test.sh
```

11/11 PASS means every surface (scope, mark, fields, mem, origins,
field-filter) works end-to-end on your install.

## Hardware-specific concerns

- macOS 14+ required (the metal-hook dylib uses `MTLCounterSamplingPoint`).
- Apple Silicon only (the hook is plain arm64; arm64e binaries skip
  injection — see `smeltr record` output).
- Hardened-runtime Python (`/Library/Frameworks/Python.framework/...`)
  strips `DYLD_INSERT_LIBRARIES`. Use Homebrew Python or a fresh venv
  to keep the hook active.

## See also

- [`docs/usage.md`](usage.md) — full feature catalog.
- [`scripts/dogfood/README.md`](../scripts/dogfood/README.md) — smoke
  test and stress workloads.
- [GitHub issues](https://github.com/dgrauet/smeltr/issues) — file
  questions or feature requests.

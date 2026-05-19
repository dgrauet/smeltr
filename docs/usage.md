# smeltr — Usage Guide

A mental model + recipes for using smeltr in day-to-day Metal/MLX work on Apple Silicon.

## Architecture (one diagram)

```
┌─────────────────────────────────────────────────────────────────┐
│                    YOUR PYTHON / METAL PROCESS                  │
│                                                                 │
│   ┌──────────────────────────────────────────────────────────┐  │
│   │  libmetal_hook.dylib  (injected via DYLD_INSERT_LIBRARIES)│  │
│   │  → swizzles Metal API, captures command buffer lifecycle │  │
│   │  → events pushed into an SHM ring                        │  │
│   └──────────────────────────────────────────────────────────┘  │
└──────────────────────────────┬──────────────────────────────────┘
                               │ shm ring (shared memory)
                               ▼
┌─────────────────────────────────────────────────────────────────┐
│                       smeltrd (daemon)                          │
│                                                                 │
│   • Drains the ring                                             │
│   • Writes ~/.smeltr/sessions/<id>/events.cbor.zst              │
│   • Exposes a UNIX socket ($TMPDIR/smeltr.sock)                 │
└──────────┬────────────────────────────┬─────────────────────────┘
           │ socket                     │ disk
           ▼                            ▼
   ┌───────────────┐         ┌─────────────────────┐
   │ smeltr tui    │         │ smeltr sessions ... │
   │ smeltr mcp    │         │ smeltr analyze      │
   │ (live)        │         │ (replay)            │
   └───────────────┘         └─────────────────────┘
```

Three actors:
- **Producer** — your target process, with `libmetal_hook.dylib` injected. Captures Metal command buffers and emits events.
- **smeltrd** — long-running daemon. Drains the ring, persists sessions to disk, exposes a socket.
- **Consumers** — `smeltr tui`, `smeltr mcp`, `smeltr sessions`, `smeltr analyze`. Read from the daemon (live) or from disk (replay).

## The three usage modes

### 1. One-shot — capture a single run

Best for: a quick experiment, a single benchmark, a crash repro.

```bash
smeltr record python my_inference.py
```

`smeltr record` will:
1. Spawn `smeltrd` if not already running.
2. Inject `libmetal_hook.dylib` into the target process via `DYLD_INSERT_LIBRARIES`.
3. Wait for the target to exit, then flush and close the session.

The session lands in `~/.smeltr/sessions/<timestamp>-<id>/`.

### 2. Always-on — persistent daemon via LaunchAgent

Best for: regular dogfooding, multiple back-to-back runs, leaving the TUI or MCP server attached across sessions.

```bash
# One-time install:
smeltr daemon install

# Then anytime:
smeltr record python run_A.py
smeltr record python run_B.py
```

The LaunchAgent (`~/Library/LaunchAgents/com.smeltr.daemon.plist`):
- Starts `smeltrd` at every login.
- Restarts it on crash (`KeepAlive=true`, `ThrottleInterval=5s`).
- Logs to `~/.smeltr/smeltrd.log`.

To uninstall:
```bash
smeltr daemon uninstall
```

### 3. Analyze — exploit recorded sessions

| Tool | When | What |
|---|---|---|
| `smeltr tui` | During or after a run | Live UI: event feed, timeline, queue depth, MLX memory; press `K` to toggle a rolling top-5 hot-kernels panel |
| `smeltr sessions list` | After | List sessions on disk |
| `smeltr sessions show <id>` | After | One-line per event-kind summary |
| `smeltr analyze <id>` | After | Run analyzer rules → findings (queue pressure, crash correlation, etc.) |
| `smeltr breakdown [--last] [<id>]` | After | Per-module + per-op GPU time breakdown for an MLX inference session |
| `smeltr mcp` (in Claude) | After | Query sessions from a Claude conversation via MCP tools |

### Breakdown — recipe

```
smeltr record python my_inference.py
smeltr breakdown --last --top-ops 10
```

Output: a tree of MLX modules with their cumulative GPU time, and
under each leaf an indented list of the top-N kernels that ran during
that module's evaluations:

```
Transformer                          1     45.300us self    45.300us subtree   30.2%
  Linear                             1     18.100us self    18.100us subtree   12.0%
    └ op:K_3900_128x33x1             3      6.200us
    └ op:K_5b00_0x0x0                1      1.500us
```

Kernel names are synthetic: `K_<pso_hash>_<tg_w>x<tg_h>x<tg_d>`.
`<pso_hash>` is the bottom 16 bits of the kernel's
`MTLComputePipelineState` pointer; threadgroup dims help distinguish
the same kernel launched with different shapes. `0x0x0` indicates an
indirect dispatch (count computed at runtime by the GPU).

Useful flags on `smeltr breakdown`:

| Flag | Effect |
|---|---|
| `--top-ops N` | Max kernels shown per module leaf (default 5). |
| `--no-ops` | Hide kernel lines (module tree only). |
| `--ops-flat` | Flat cross-module kernel table instead of the tree. |
| `--flamegraph out.svg` | Folded-stack flamegraph SVG (via `inferno`). |
| `--chrome-trace out.json` | Chrome Trace Event Format, open in Perfetto or Speedscope. |

When a forward pass collapses into a single top-level MLX eval (common
for autoregressive LLMs), all module attributions land under
`<unscoped>` — but the op-level breakdown under `<unscoped>` still
gives you the kernel-level decomposition for that bucket.

Kill switch: `SMELTR_HOOK_NO_OPS=1` disables op-level capture entirely
(module-level breakdown stays active). See the
[ADR](adr/0002-op-attribution-pso-signature.md) for the design
rationale behind PSO-signature naming and stage-boundary timing.

#### MCP equivalents

- `get_inference_breakdown` — returns the full `ModuleBreakdown` tree
  including ops. Accepts `max_depth`, `top_n`, `min_gpu_ns`,
  `include_ops`, `top_ops_per_leaf`.
- `get_op_summary` — flat list of kernel signatures with GPU time and
  percentage, aggregated across all module leaves.

### Profiling scopes

Annotate code blocks with `smeltr.scope("name")` to attribute Metal kernel
time to semantic regions instead of relying on `mlx.nn.Module` class names
alone. Scopes nest freely and interleave with module-call tracking.

```python
import smeltr
smeltr.attach()

@smeltr.scope("denoise.guided_step")
def guided_step(...): ...

with smeltr.scope("denoise.pass:cond"):
    cond_x0 = model(**cond_kwargs)
    mx.core.eval(cond_x0)  # eval must occur inside the scope
```

`get_inference_breakdown` then returns a tree where `denoise.pass:cond`
appears as a node with its rolled-up `gpu_ns_subtree`, `kernel_count`,
and top kernel ops.

**Important:** kernels are attributed by the `module_stack` snapshot
taken at `mx.core.eval()` time. If your eval (or implicit eval at array
materialization) happens *outside* the `with smeltr.scope(...)` block,
the kernels go into `unscoped_gpu_ns`. The general pattern is "compute
and materialize inside the scope".

**Scopes are thread-local.** A scope active on the thread that opened the
`with` block is invisible to work submitted to other threads (e.g. a
`ThreadPoolExecutor`). Open a scope inside the worker if you need
attribution there.

**Async and generators:** the decorator form (`@smeltr.scope("...")`) is
rejected for `async def` functions and generator functions, because the
scope would enter and exit before the coroutine or generator body runs.
Use the `with` form inside the body instead.

### Symbolic kernel names

The Metal hook captures each Compute Pipeline State Object's underlying
function name at creation time (`MTLDevice newComputePipelineStateWithFunction:error:`).
Op rows in `get_inference_breakdown` and `get_op_summary` carry two
enrichment fields:

- `symbol` — the MLX shader name, e.g. `gemm_t_n_bf16_64_64_32_2_2_8`.
- `kind` — a canonical op label derived from `symbol`, e.g. `Matmul`.
  Computed via a static pattern table in `smeltr-analyzer`; returns
  `None` when no pattern matches.

`name` (the legacy `K_<pso_hash>_<wxhxd>` fingerprint) is preserved as a
secondary identifier — useful for distinguishing same-kind dispatches
that share a `symbol` but differ by tile size.

The pattern table is MLX-version-sensitive; new MLX releases may add
shaders not yet covered. Unknown symbols still surface as `symbol`
without a `kind`.

### Naming sessions

Label a session via `SMELTR_SESSION_NAME` or `smeltr record --name`:

```bash
SMELTR_SESSION_NAME="ltx2-baseline-480x704x33" ./pipeline.py
smeltr record --name "ltx2-batched-cfg" -- ./pipeline.py
```

`smeltr session ls` shows the name (when set) as a `name="..."` suffix.
`list_sessions` (MCP) surfaces it as the `name` field per session.

Any MCP tool or CLI command that takes a session id (short id, full
UUID) also accepts the name. On collision (multiple sessions sharing
a name), the **most recent** wins — use the short id when you need a
specific older session.

**Validation:** names are trimmed and capped at 200 chars; inputs
containing NUL, control characters, or `/` are silently dropped (the
session records with no name and a warning is logged). The session
directory format (`YYYY-MM-DD-HHMMSS-<8hex>`) is unchanged — the name
is metadata only.

### Exporting sessions

Dump a recorded session to chrome-trace JSON for visual analysis:

```bash
smeltr export ltx2-baseline --format chrome-trace --output trace.json
# then open trace.json in chrome://tracing, Perfetto, or Speedscope
```

Equivalent paths:

- **CLI:** `smeltr export <session-ref> [--format chrome-trace|json] [--output PATH]`
  (default format chrome-trace, default output `<short_id>.json`, use `-` for stdout).
- **MCP:** `export_session(session, format, output_path)` writes the
  file and returns its path.
- **Python:** `smeltr.export(filepath, format="chrome-trace", session=None)`.
  With no `session`, uses the active session known to the connected
  daemon. Drop it in an `atexit` or `finally` block to dump traces at
  end of a CI run.

The chrome-trace output uses three swimlanes:

- **Python** — user scopes (`smeltr.scope(...)` and `mlx.nn.Module`
  calls) and instant marks.
- **Metal CBs** — command-buffer commits with `in_flight_ns` duration,
  grouped by queue (`tid="queue_<id>"`).
- **Kernels** — per-op events labeled by `symbol` (the MLX shader name)
  when available, falling back to the legacy `K_xxxx_AxBxC` fingerprint.
  Grouped by CB (`tid="cb_<id>"`).

Sessions without a `symbol` (recorded before symbolic kernel name
capture landed) export with only the fingerprint name on the Kernels
lane — still useful, just less readable.

### Diffing sessions

Compare two recorded sessions to surface scope-level and op-kind
deltas — useful for spotting which scope an optimization (or
regression) hit:

```bash
smeltr compare ltx2-baseline ltx2-batched-cfg --top 10
```

Output sections (each capped at `--top N`, default 20):

- **SCOPE DELTAS** — `qualname` present in both sessions, sorted by
  `|delta_gpu_ns|` descending. Both parents and leaves of the scope
  tree appear independently.
- **OP KIND DELTAS** — ops aggregated cross-session by canonical
  `kind` (Matmul, ScaledDotProductAttention, …), falling back to
  `symbol` and then raw name when no `kind` resolves.
- **SCOPES ONLY IN A / ONLY IN B** — scopes whose qualname appears in
  one session but not the other (e.g. an "E2 skip" pass dropped
  between iterations).

The MCP `compare_sessions` tool returns the same four arrays in its
response (`scope_deltas`, `op_deltas`, `scopes_only_in_a`,
`scopes_only_in_b`) on top of the legacy `a`/`b`/`delta` stats.

All times in seconds; deltas show the sign (`+` slower, `-` faster)
and percentage (`(n/a)` when the baseline is zero).

### Memory tracking

Per-scope GPU memory usage helps debug
`kIOGPUCommandBufferCallbackErrorImpactingInteractivity` watchdog
OOMs by surfacing which scope hit peak memory.

```bash
smeltr memory ltx2-baseline --top 10
```

Two sections:

- **SCOPE PEAK MEMORY** — `MTLDevice.currentAllocatedSize` sampled at
  CB Committed and CB Completed for every kernel dispatch.
  Aggregated per scope as peak / avg / end / sample count.
- **HEAP PEAK** — for each scope, the maximum number of live
  `MTLHeap` objects (and their total `size_bytes`) seen during the
  scope window. Derived from `MetalHeapAlloc/Free` events.

The MCP `get_memory_breakdown` tool returns the same two arrays.

Memory comparison: `smeltr compare` and the MCP `compare_sessions`
tool now include a `MEMORY DELTAS` section / `memory_deltas` field
showing per-scope peak deltas — useful for confirming an
optimization reduced peak memory (or that a regression bumped it).

**Limitations:** v1 walks events single-threaded (no per-tid
attribution). MLX is typically single-thread on the producer side so
this matches real workloads. Multi-thread cases may misattribute
samples to the wrong scope.

### Dispatch origins

Map kernel dispatches back to the Python source file:line that
triggered them. Useful for confirming "this Matmul came from
`attention.py:127`" — combined with op kinds and scopes, you get
full top-down attribution from a user code line to GPU time.

```bash
SMELTR_STACK_CAPTURE=1 smeltr record -- ./pipeline.py
smeltr origins ltx2-baseline --top 10
```

The `SMELTR_STACK_CAPTURE=1` env var is **opt-in** because the stack
walk adds ~1–5 µs per `mx.eval`. Without it the session works as
normal and `smeltr origins` shows an empty table with a hint.

Output: per-(kind, file:line), sum GPU time + dispatch count.

- **CLI:** `smeltr origins <session-ref> [--top N]`.
- **MCP:** `get_dispatch_origins(session)` returns the same list.
- **Compare:** `smeltr compare` and the MCP `compare_sessions` tool
  include an `ORIGIN DELTAS` section / `origin_deltas` field showing
  per-(kind, file:line) GPU time deltas between two sessions.

The top non-smeltr Python frame is used for attribution; deeper
frames are still recorded in the event log but not aggregated.
File names are reduced to basename (`attention.py:127`) so moves
keep grouping intact; renaming functions loses correlation.

## Typical workflow

```
[once]      smeltr daemon install              ← persistent daemon
[per run]   smeltr record <your-cmd>           ← capture
[live]      smeltr tui                         ← watch in real time (optional)
[analysis]  Ask Claude: "list my smeltr sessions" or "compare A and B"
            → Claude calls the smeltr MCP tools
```

## Getting smeltr on your PATH

Symlink the release binaries into a PATH directory:

```bash
mkdir -p ~/.local/bin
ln -sf $(pwd)/target/release/smeltr  ~/.local/bin/smeltr
ln -sf $(pwd)/target/release/smeltrd ~/.local/bin/smeltrd
```

`~/.local/bin` is already on the PATH on most setups. The symlinks follow the build target, so `cargo build --release` automatically updates them.

## MCP integration (Claude Code / Claude Desktop)

Add to `~/.claude.json` (under `mcpServers`):

```json
"smeltr": {
  "type": "stdio",
  "command": "/Users/<you>/.local/bin/smeltr",
  "args": ["mcp"],
  "env": {}
}
```

From any Claude session, you can then ask things like:
- "List my smeltr sessions"
- "Compare sessions A and B"
- "Find correlations around the queue depth peak in session X"
- "Get the crash report for the last session"

### Tool catalog (for agents)

Every tool accepts a session ref as short id (8 hex), full UUID, or
`SessionMetadata.name` (see [Naming sessions](#naming-sessions)).

| Tool | Use when | Returns |
|---|---|---|
| `list_sessions` | Starting point: enumerate available sessions | short_id, full_id, name, started/ended, exit_code, event_count, root_cause_title |
| `get_session_summary` | Quick overview of one session | counts, time range, root cause |
| `query_events` | Raw event stream with filters | events filtered by source, kind, limit |
| `find_correlations` | Deterministic analyzer findings | Correlated events around anomalies |
| `get_crash_report` | Session had a crash | parsed crash dumps |
| `get_metal_cb_history` | Inspect Metal command-buffer activity | CB committed/scheduled/completed events |
| `get_inference_breakdown` | Per-scope GPU time tree | Hierarchical: scope → child scopes + ops |
| `get_op_summary` | Flat cross-scope op stats | Aggregated by op `kind` (Matmul, SDPA, …) |
| `get_memory_breakdown` | Per-scope memory peak/avg/end + heap | `scope_memory`, `heap_memory` arrays |
| `get_dispatch_origins` | Per-(kind, file:line) attribution | requires `SMELTR_STACK_CAPTURE=1` at record time |
| `compare_sessions` | A/B regression analysis | `scope_deltas`, `op_deltas`, `memory_deltas`, `origin_deltas`, scopes-only-in-A/B |
| `export_session` | Dump for external viewer | writes chrome-trace JSON (or raw JSON) to `output_path` |

### Typical agent workflow

For "find the bottleneck in a 30-step CFG denoising loop":

1. `list_sessions` → pick recent session by name or short_id.
2. `get_inference_breakdown` → identify the top-level scope dominating GPU time (e.g. `denoise.guided_step`).
3. `get_op_summary` → see which op kinds dominate (e.g. `Matmul` 4.31s, `ScaledDotProductAttention` 1.72s).
4. `get_dispatch_origins` (if session was recorded with `SMELTR_STACK_CAPTURE=1`) → pin the dominant Matmul to a specific `attention.py:127`.
5. After implementing an optimization, record a second session, then `compare_sessions` between baseline and optimized → confirm `scope_deltas` / `op_deltas` / `memory_deltas` shifted in the expected direction.
6. Optionally `export_session --format chrome-trace` and open in chrome://tracing or Perfetto for visual inspection.

## Files & directories

| Path | Purpose |
|---|---|
| `~/.smeltr/sessions/<id>/events.cbor.zst` | Captured event stream |
| `~/.smeltr/sessions/<id>/metadata.toml` | Session metadata (argv, start/end times, …) |
| `~/.smeltr/smeltrd.log` | Daemon logs (managed by LaunchAgent) |
| `~/.smeltr/smeltrd.pid` | Current daemon PID |
| `$TMPDIR/smeltr.sock` | UNIX socket the daemon listens on |
| `~/Library/LaunchAgents/com.smeltr.daemon.plist` | LaunchAgent definition |

## Common pitfalls

### Dev dylib override

For development against an uninstalled dylib build:

```bash
SMELTR_DYLIB=$(pwd)/metal-hook/build/libmetal_hook.dylib smeltr record python my_inference.py
```

`smeltr` ships its own copy of `libmetal_hook.dylib` embedded in the binary,
so end users never need to set `SMELTR_DYLIB`. A `cargo build --release`
rebuilds both halves together — no version skew is possible.

### Two `smeltrd` processes running

Symptom: `ps aux | grep smeltrd` shows two PIDs.

Cause: an orphan daemon (from a manual `smeltrd &`) is still running alongside the LaunchAgent-managed one.

Fix: identify the LaunchAgent-managed one (`launchctl list com.smeltr.daemon` returns its PID) and kill the other(s).

### Socket-path errors on macOS

If you see `bind: invalid argument` and your `SMELTR_SOCKET` env var is set to something like `$XDG_RUNTIME_DIR/smeltr.sock`: unset it. `XDG_RUNTIME_DIR` is a Linux convention and is empty on macOS, which collapses the path to `/smeltr.sock` (too long after expansion or invalid). Default is `$TMPDIR/smeltr.sock`, which works.

## See also

- `docs/dogfood-findings.md` — real-world findings from production validation
- `README.md` — project overview
- `docs/adr/` — architecture decision records

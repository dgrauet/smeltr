# smeltr

[![CI](https://github.com/dgrauet/smeltr/actions/workflows/ci.yml/badge.svg)](https://github.com/dgrauet/smeltr/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Metal/MLX observability and crash post-mortem for macOS Apple Silicon.

**Status:** v1 — feature complete, dogfooded against real MLX inference workloads.

## What it does

`smeltr` records what's happening inside the GPU while your Metal or MLX workload runs, persists each run as a queryable session, and surfaces correlations between GPU command-buffer pressure, memory allocations, thermal state, and (when it happens) crashes.

```
your-python ──► libmetal_hook.dylib ──► shm ring ──► smeltrd ──► ~/.smeltr/sessions/
                (DYLD_INSERT_LIBRARIES)                  │
                                                         ├──► smeltr tui   (live)
                                                         ├──► smeltr mcp   (Claude)
                                                         └──► smeltr analyze
```

See **[`docs/usage.md`](docs/usage.md)** for the user-facing guide (architecture, three usage modes, MCP integration, common pitfalls).

## Requirements

- macOS 14+ on Apple Silicon (M1/M2/M3/…).
- Rust 1.88+ (pinned via `rust-toolchain.toml`).
- Python 3.10+ if you want the optional MLX sidecar (`pip install -e 'python/[mlx,dev]'`).

## Quick start

```bash
git clone <repo> smeltr && cd smeltr
cargo build --release

# Put the binaries on $PATH
mkdir -p ~/.local/bin
ln -sf "$PWD/target/release/smeltr"  ~/.local/bin/smeltr
ln -sf "$PWD/target/release/smeltrd" ~/.local/bin/smeltrd

# Install the persistent daemon (LaunchAgent, auto-restart)
smeltr daemon install

# Capture a run (optionally name it for later lookup)
smeltr record --name "baseline" -- python my_inference.py

# Watch live
smeltr tui

# Or query from Claude via MCP — see docs/usage.md
```

For semantic GPU-time attribution from MLX code, the optional Python sidecar
adds `smeltr.scope("name", **fields)` and `smeltr.mark("label", **fields)`.
It also auto-tracks every `safetensors.safe_open` / `mlx.core.load` call as
a `ModelLoad` event — surfaces "model loaded twice" bugs in the TUI (key `M`),
in `smeltr analyze` (rule `duplicate-model-load`), in chrome-trace
(swim lane + per-model counter), and via MCP (`get_model_loads`).
See [`docs/usage.md`](docs/usage.md) and the
[migration guide](docs/migration-from-bespoke-profilers.md) for moving off
bespoke profilers.

The `libmetal_hook.dylib` is built and **embedded into the `smeltr` binary** at compile time (via `crates/smeltr-cli/build.rs` invoking `make -C metal-hook all`). End users never need to set `SMELTR_DYLIB` or manage the dylib path.

## What gets captured

`smeltrd` runs seven probes by default:

| Probe | What it captures |
|---|---|
| `vm` | wired / active / compressed memory, swap, page-out rate |
| `proc` | top-N CPU; flags `ReportCrash` / `diagnosticservicesd` / `UserNotificationCenter` / `spindump` when above threshold |
| `thermal` | `kern.thermalstate` (Nominal/Light/Moderate/Heavy) |
| `oslog` | GPU subsystems + kernel "GPU watchdog" messages via `/usr/bin/log stream` |
| `ioreport` | v1 stub — real IOReport residency lands in a future plan |
| `crash-reports` | parses `.ips` files dropped in `~/Library/Logs/DiagnosticReports/` |
| `mach-exceptions` | attached only to children spawned by `smeltr record` (same-UID PIDs) |

The Metal hook adds: `MetalCbCommitted`, `MetalCbScheduled`, `MetalCbCompleted` (with status, error code/domain, `in_flight_ns`), `MetalCbWarning` (CBs in-flight > 5s), `MetalHeapAlloc`/`Free`, `MetalBufferAlloc`/`Free`, `MetalTextureAlloc`/`Free`.

## SIP / hardened binaries

Hardened binaries (Apple-shipped Python on Sonoma/Sequoia, e.g. `/usr/bin/python3`) reject `DYLD_INSERT_LIBRARIES` due to SIP. `smeltr record` detects this via `codesign --display --verbose=2` (looking for the `runtime` flag) and falls back to no-hook automatically with a stderr warning.

Workaround: use a Homebrew-installed Python (`brew install python`), or any Python launched via a non-hardened wrapper, to keep the hook active.

Kill switch: `SMELTR_HOOK_DISABLE=1` makes the loaded dylib inert.
`SMELTR_HOOK_NO_OPS=1` disables op-level GPU capture only (CB-level capture
stays on). Use when counter sampling overhead is undesirable.

## CLI reference

| Command | Purpose |
|---|---|
| `smeltr record [--name N] -- <cmd>` | Capture a run (optionally named) |
| `smeltr mark <label> [--field k=v]` | Append a marker event to the active session |
| `smeltr tui` | Live event feed / timeline |
| `smeltr sessions ls` | List sessions on disk (annotates ambient/scoped) |
| `smeltr sessions show <id>` | Per-event-kind summary |
| `smeltr analyze [<id>]` | Run analyzer rules → findings |
| `smeltr breakdown [<id>] [--field k=v]` | Per-module GPU time breakdown (filterable by scope field) |
| `smeltr memory [<id>]` | Per-scope MTLDevice memory peak/avg/end + heap |
| `smeltr origins [<id>]` | Per-(kind, file:line) GPU attribution (needs `SMELTR_STACK_CAPTURE=1`) |
| `smeltr compare <id-a> <id-b>` | A/B regression: scope + op-kind GPU deltas |
| `smeltr export <id> --format chrome-trace\|json` | Dump to Perfetto / Speedscope / raw JSON |
| `smeltr doctor` | Audit probe availability and permissions |
| `smeltr mcp` | Stdio MCP server (Claude integration) |
| `smeltr daemon install` | Install persistent LaunchAgent |

Session refs accept the short id (last 8 hex), the full UUID, or the
`--name` you passed to `record` (most-recent-wins on name collision).

### Session kinds

`smeltrd` writes events into one of two kinds of session:

- **Ambient** — opened automatically when the daemon starts. Captures all
  events not scoped to a specific PID: system probes, thermal samples,
  unattributed crash reports, etc. One ambient session per daemon lifetime.
- **Scoped** — opened by `smeltr record` for the child process it spawns.
  Every event tagged with that child's PID lands in this session. Closed
  with an exit-code marker when the child terminates.

By default, `smeltr breakdown --last` and `smeltr analyze --last` pick the
most-recent scoped session. Pass `--include-ambient` to revert to the
older behaviour (newest of any kind).

`smeltr sessions ls` annotates each line with `[ambient]` or
`[scoped pid=N cmd=...]`.

### `smeltr breakdown [--last] [<session_id>]`

Per-module GPU time breakdown for an MLX inference session. Pure observation
- no synchronous evaluation is forced.

Flags:

- `--last` - prefer the most-recent post-mortem session (otherwise newest).
- `--top N` - limit the table to N rows (default 20).
- `--depth N` - cut the tree at depth N (default 6).
- `--field key=value` - keep only subtrees whose `ModuleEntered.fields`
  match (recursively). Repeatable; values are type-inferred (bool/int/float/string).
- `--flamegraph <out.svg>` - write a folded-stack flamegraph SVG.
- `--chrome-trace <out.json>` - write a Chrome Trace Event Format file
  (open in Perfetto or Speedscope).

For semantic module-level attribution, wrap the forward pass with the
Python sidecar's `smeltr.scope("name", **fields)` context manager. Without
sidecar scopes, the op-level decomposition below still ranks top kernels
by PSO signature.

### Op-level decomposition (Phase 2.5)

Under each module leaf, `smeltr breakdown` lists the top-N GPU kernels
captured by tracking each dispatch's MTLComputePipelineState pointer
and threadgroup dimensions. Names are synthetic — `K_<pso_hash>_<tg_w>x<tg_h>x<tg_d>` —
because MLX 0.31 does not emit debug groups in its kernel encoding
path. Same-named entries across CBs correspond to the same underlying
kernel; threadgroup dimensions disambiguate when a single PSO is
launched with different shapes.

Per-kernel GPU time is approximated by distributing the per-CB
`in_flight_ns` pro-rata by dispatch count across the kernels that ran
in that CB. This is an order-of-magnitude estimate: it assumes all
dispatches within a CB cost the same. For typical workloads where one
or two kernels dominate, the ranking is informative even if the
absolute numbers per kernel are not exact.

Useful when a forward pass collapses into a single top-level `mx.eval()`
(typical for autoregressive LLMs): the module-level attribution shows
100% under `<unscoped>`, but the op-level view decomposes that bucket
by kernel signature.

Flags:

- `--top-ops N` (default 5) — max kernels shown per module leaf.
- `--no-ops` — hide the per-kernel lines.
- `--ops-flat` — cross-module flat table instead of the module tree.

Kill switch: `SMELTR_HOOK_NO_OPS=1` disables op-level capture (CB-level
capture stays active).

## Development

```bash
# Workspace test + lint
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Python sidecar
pip install -e 'python/[mlx,dev]'
pytest python/tests/

# Skip the dylib rebuild during cargo build (CI / cross-compile)
SMELTR_SKIP_DYLIB_BUILD=1 cargo build --release

# Rebuild the dylib alone (rare)
make -C metal-hook clean all
```

Conventional Commits are enforced via `commitlint.config.js` + pre-commit hooks.

## Repo layout

- `crates/` — Rust workspace (18 crates): core, daemon, CLI, analyzer, replay, TUI, MCP server, probes.
- `metal-hook/` — ObjC++ dylib injected via `DYLD_INSERT_LIBRARIES`.
- `python/` — opt-in Python sidecar (`smeltr` package, pip-installable).
- `docs/` — usage guide, ADRs, dogfood findings.

## License

MIT — see [`LICENSE`](LICENSE).

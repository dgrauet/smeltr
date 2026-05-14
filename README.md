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

# Capture a run
smeltr record python my_inference.py

# Watch live
smeltr tui

# Or query from Claude via MCP — see docs/usage.md
```

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

- `crates/` — Rust workspace (12+ crates): core, daemon, CLI, analyzer, replay, TUI, MCP server, probes.
- `metal-hook/` — ObjC++ dylib injected via `DYLD_INSERT_LIBRARIES`.
- `python/` — opt-in Python sidecar (`smeltr` package, pip-installable).
- `docs/` — usage guide, ADRs, dogfood findings.

## License

MIT — see [`LICENSE`](LICENSE).

# CLAUDE.md — smeltr

Project: Metal/MLX observability tool for macOS Apple Silicon.

## Layout

- `crates/` — Rust workspace (12+ crates) : `smeltr-core` (event model + zstd
  session writer/reader), `smeltr-daemon` (Unix socket server +
  bus + flight recorder + triggers), `smeltr-cli` (`smeltr` binary),
  `smeltr-analyzer` (deterministic rules), `smeltr-replay`, `smeltr-tui`
  (ratatui), `smeltr-mcp` (rmcp stdio MCP server), `smeltr-probes-*` (system
  probes), `smeltr-metal-ring` (mmap ring writer/reader), `smeltr-metal-harness`.
- `metal-hook/` — ObjC++ dylib injected via `DYLD_INSERT_LIBRARIES`.
- `python/` — opt-in Python sidecar (`smeltr` package, pip-installable).
- `docs/` — ADRs + handbook (planning docs live outside the repo per the
  global Claude instruction, in `~/Work/.superpowers/tools/{specs,plans}/`).

## Conventions (NON-NEGOTIABLE)

- TDD strict : failing test → minimal code → green → commit.
- `cargo fmt --all` + `cargo clippy --workspace --all-targets -- -D warnings`
  must pass after every commit.
- `#[serial_test::serial]` on env-mutating tests (`SMELTR_HOME` / `SMELTR_SOCKET`).
- No `unwrap` / `expect` outside `main.rs` and tests.
- Conventional commits : `<type>(<scope>): <description>` — types `feat`,
  `fix`, `chore`, `docs`, `test`, `refactor`, `ci`, `style`, `perf`, `build`, `revert`.
  (commitlint default config-conventional set; `ci+test:` style multi-type subjects fail.)
- Python CI runs `ruff format --check` AND `ty check` (Astral type checker, NOT mypy).
  `ty` does not honor `# type: ignore[...]` — use `cast(T, x)` or fix the type properly.
  Locally before push: `cd python && .venv/bin/ruff format .`
- New workspace members are added to root `Cargo.toml` ONLY when the
  directory exists.

## Adding features

- **Additive schema fields**: use `#[serde(default, skip_serializing_if = "Option::is_none")]`
  (or `Vec::is_empty`) so pre-existing sessions decode cleanly. Required pattern; see
  `OpSample.symbol`, `SessionMetadata.name`, `MlxEvalEntered.stack_frames`.
- **Adding a `Payload` variant**: `cargo build --workspace --tests 2>&1 | grep "error\["` lists
  every literal-construction site that needs the new field. Rust requires explicit fields on
  literal construction even when `#[serde(default)]` is set on the type.
- **Ring wire format change**: bump `RING_VERSION` in BOTH `crates/smeltr-metal-ring/src/wire.rs`
  AND `crates/smeltr-metal-ring/include/smeltr_ring.h` — `header_matches` parity test enforces.
  Update BOTH `metal-hook/src/ring.c` (C writer used by the dylib) AND
  `crates/smeltr-metal-ring/src/writer.rs` (Rust writer used by the daemon) — independent
  implementations of the same byte layout.
- **Session ref resolution**: `smeltr_mcp::types::resolve_session(arg)` accepts short id (8 hex
  suffix), full UUID, or `SessionMetadata.name` (most-recent-wins on collision). Use it in every
  new tool that takes a session.
- **New MCP tool**: file in `crates/smeltr-mcp/src/tools/<name>.rs` with `Params`/`Response`/`run`,
  register `pub mod` in `tools.rs`, add dispatch arm in `server.rs::call_tool` AND a
  `tool::<Params>(...)` entry in `list_tools()`.
- **New CLI subcommand**: file in `crates/smeltr-cli/src/commands/<name>.rs` with
  `pub fn run(...) -> anyhow::Result<()>` + `pub(crate) fn render(...) -> String` (testable
  without a child process), register `pub mod` in `commands/mod.rs`, add `Cmd::Xxx` variant +
  sync dispatch arm in `main.rs`.
- **Routing for sidecar events**: Python sidecar events are routed by `scope_token`
  (env-stamped UUID) first, then PID, then ambient. Any new client emitting via
  the daemon socket should pass `scope_token` from `SMELTR_SCOPE_TOKEN` if set,
  or routing will fall back to PID match (which breaks under `uv`/`poetry`
  launchers — see #31).

## Build

```bash
cargo build --workspace
make -C metal-hook clean all      # ObjC++ dylib
pip install -e 'python/[mlx,dev]' # Python sidecar (or uv pip install)
```

## Test

```bash
cargo test --workspace
python -m pytest python/tests/
intendant audit .                                    # governance audit
```

## Wire protocol

CBOR length-prefixed frames over a Unix socket. See
`crates/smeltr-core/src/codec.rs` and
`crates/smeltr-daemon/src/protocol.rs`.

## Env vars

- `SMELTR_HOME` — sessions root (default `~/.smeltr`).
- `SMELTR_SOCKET` — daemon socket path (default `$XDG_RUNTIME_DIR/smeltr.sock`).
- `SMELTR_RING_PATH` — metal-hook mmap ring file (set by `smeltr record`).
- `SMELTR_DYLIB` — override path to `libmetal_hook.dylib` (dev override; smeltr ships an embedded copy by default).
- `SMELTR_HOOK_DISABLE=1` — kill switch for the metal-hook dylib.
- `SMELTR_HOOK_FORCE_OS_MAJOR=<n>` — simulate a macOS major version
  (test override; on macOS < 14 the hook auto-skips).
- `SMELTR_HOOK_RECALIBRATE_SEC=<n>` — opt-in periodic ticks→ns
  recalibration interval (EMA, alpha=0.2). Off by default; useful on
  multi-hour sessions where thermal drift can move the CPU/GPU tick
  ratio. Sanity-rejected samples emit a throttled `MetalHookSkipped`
  diagnostic.
- `SMELTR_HOOK_DISPATCH_BOUNDARY=1` — opt-in per-dispatch GPU timing on
  M3+ devices that expose `MTLCounterSamplingPointAtDispatchBoundary`.
  Replaces the encoder-level stage-boundary + pro-rata attribution with
  exact per-dispatch ns. Auto-falls-back to stage-boundary on M1/M2 (or
  on sustained sample-buffer alloc failure).
- `SMELTR_HOOK_ML_ENCODER=1` — opt-in MTL4 machine-learning encoder
  visibility (macOS 26). Swizzles `dispatchNetworkWithIntermediatesHeap:`
  on `_MTL4MachineLearningCommandEncoder` (and Debug/Tools variants) to
  record one dispatch per network. Emits `K_MLNet_<encoder_addr>` in the
  op breakdown. `setPipelineState:` is deliberately NOT swizzled (Apple's
  ML proxy machinery crashes if it is).
- `SMELTR_SCOPE_TOKEN` — UUID stamped by `smeltr record` into the child env. The
  Python sidecar reads it at `attach()` and tags every Emit so the daemon
  routes the event to the correct scoped session even when the recorded
  command is a launcher (`uv run`, `poetry run`, `python -m foo`, shell
  wrapper) and the grandchild PID differs from the spawned child PID.
  Internal plumbing — not for end-user manual override.
- `SMELTR_SESSION_NAME` — user-facing session name (validated: cap 200, no NUL/control/`/`);
  surfaced by `list_sessions` and accepted as an alias by every CLI/MCP session arg via
  `smeltr_mcp::types::resolve_session`.
- `SMELTR_STACK_CAPTURE=1` — opt-in: capture top 3 Python frames at each `mx.eval`
  (~1-5 µs/eval). Fills `MlxEvalEntered.stack_frames`; consumed by `smeltr origins` /
  `get_dispatch_origins`.

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
  `fix`, `chore`, `docs`, `test`, `refactor`.
- New workspace members are added to root `Cargo.toml` ONLY when the
  directory exists.

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

# ADR-0001 — Initial architecture: daemon + probes + hook + sidecars

**Date:** 2026-05-14
**Status:** Accepted

## Context

We need a Metal/MLX observability tool for macOS Apple Silicon that captures
crash watchdogs and produces deterministic post-mortem analysis. Existing
tools (`mactop`, `asitop`, `macmon`, `powermetrics`) show system state but do
not instrument Metal/MLX deeply: no `MTLCommandBuffer` timestamping, no
`MTLHeap` tracking, no correlation between Metal events and MLX pipeline,
no automatic crash report capture.

## Decision

A daemon-centric architecture with lightweight frontends:

- **`smeltrd`** — persistent Rust daemon that owns all probes, aggregates
  events into sessions on disk, exposes a Unix socket to which CLI/TUI/MCP
  clients connect. Passive capture remains possible even if the TUI is not
  open at crash time.
- **Probes** as separate workspace crates behind a common `Probe` trait,
  each supervised with exponential backoff: `vm`, `proc`, `thermal`,
  `ioreport`, `oslog`, `mach-exceptions`, `crash-reports`, `metal-hook`.
- **Metal hook** as an ObjC++ dylib injected via `DYLD_INSERT_LIBRARIES`,
  swizzling `MTLDevice`/`MTLCommandQueue`/`MTLCommandBuffer`/`MTLHeap`/
  `MTLBuffer`/`MTLTexture`. SPSC mmap ring for the hot-path back to the
  daemon.
- **Python sidecar** as an opt-in pip-installable package. Wraps
  `mx.eval`, polls `mx.metal` memory stats, tracks `mx.array` via weakref.
  Pure socket client (no PyO3).
- **Flight recorder** — 60s in-RAM ring inside the daemon, always active.
  Flushed to a post-mortem session on three triggers: `.ips` arrival,
  `EXC_RESOURCE`/`EXC_BAD_ACCESS`, Metal command-buffer error code matching
  `kIOGPUCommandBufferCallback*`.
- **Sessions on disk** — `events.cbor.zst` (length-prefixed CBOR over zstd
  streaming) + `metadata.toml`. Reader auto-detects legacy uncompressed
  `events.cbor`.
- **Analyzer** as a separate crate with deterministic rules: each rule
  matches an event pattern and produces a contributing factor. The first
  shipped rules cover the v1 done criterion (kIOGPU error naming, queue
  depth, MLX eval timing, system pressure).
- **TUI and MCP** as separate crates consuming the same broadcast bus
  (live) or the same session reader (replay/post-mortem). The TUI uses
  ratatui; the MCP server uses rmcp stdio.

## Consequences

**Positive:**
- Probes are independently developable and testable.
- Crash capture works without active user action (flight recorder).
- All consumers (CLI, TUI, MCP) read the same event log and metadata
  contract.
- The Python sidecar is opt-in: `smeltr record python my_script.py` works
  without it, the hook + system probes already cover ~80% of the
  observability need.

**Negative:**
- Multi-process coordination via Unix sockets and mmap rings adds
  complexity vs. an in-process tool.
- macOS-only by design; Linux/Windows are not supported.
- DYLD_INSERT_LIBRARIES is blocked by SIP on hardened binaries; the
  `metal-hook` probe degrades to disabled with a clear message in that
  case.

## Status of related decisions

- ADR-0001 (this) — initial architecture.
- Future ADRs may cover: telemetry retention policy, multi-machine
  aggregation, plugin LLM analysis surface.

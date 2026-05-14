# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-05-14

### Added

- Rust workspace with foundational crates: `smeltr-core` (event model + zstd
  session writer/reader), `smeltr-daemon` (Unix socket server, bus, flight
  recorder, post-mortem triggers), `smeltr-cli` (`smeltr` binary).
- 7 system probes: `vm`, `proc`, `thermal`, `ioreport`, `oslog`,
  `mach-exceptions` (with full trailer decode), `crash-reports`.
- Metal hook dylib (ObjC++) injected via `DYLD_INSERT_LIBRARIES`: swizzles
  `MTLDevice`/`MTLCommandQueue`/`MTLCommandBuffer`/`MTLHeap`/`MTLBuffer`/
  `MTLTexture` lifecycle methods. macOS >= 14 required (auto-skips on older
  versions, emits `MetalHookSkipped`).
- `smeltr-analyzer` deterministic rules: `MetalErrorRule` (names kIOGPU
  error codes, elevates known watchdog codes to RootCause), `QueueDepthRule`,
  `MlxTimingRule`, `SystemPressureRule`.
- Python sidecar (`python/smeltr/`): `attach`/`detach`/`session`/`mark`,
  `mx.eval` tracing via `decorate_eval`, `mx.metal` memory polling,
  weakref-based `mx.array` tracking, `snapshot`, `panic_on` watchdog,
  shutdown hooks (`atexit`/`SIGTERM`/`sys.excepthook`).
- TUI (ratatui, `smeltr tui` live + `smeltr sessions open <id>` replay)
  with 5 panels: Timeline / Metal CBs / Memory / MLX sidecar / System pressure
  plus log feed.
- MCP server (`smeltr mcp`, rmcp 1.7 stdio) with 7 tools: `list_sessions`,
  `get_session_summary`, `query_events`, `find_correlations`,
  `get_crash_report`, `get_metal_cb_history`, `compare_sessions`. Each session
  exposed as `smeltr://session/<dir_name>` resource.
- CLI: `smeltr daemon {start,stop,status}`, `smeltr mark`, `smeltr sessions
  {ls,show,open}`, `smeltr doctor`, `smeltr record`, `smeltr analyze`,
  `smeltr tui`, `smeltr mcp`.

[Unreleased]: https://example.com/smeltr/compare/v0.1.0...HEAD
[0.1.0]: https://example.com/smeltr/releases/tag/v0.1.0

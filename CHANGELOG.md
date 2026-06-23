# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.12.0](https://github.com/dgrauet/smeltr/compare/v0.11.0...v0.12.0) (2026-06-23)


### Features

* **core,mcp:** format-aware reader + read_events_filtered; query_events uses the footer index ([67110d7](https://github.com/dgrauet/smeltr/commit/67110d7e2fb574e09ab17589893c6be028cd5152))
* **core:** chunked session container codec (chunked.rs) ([7d04989](https://github.com/dgrauet/smeltr/commit/7d04989f6d1d6f42dafa53097368cfe4329f909f))
* **core:** opt-in chunked session format with (source, t_bucket) index ([69bfd4d](https://github.com/dgrauet/smeltr/commit/69bfd4ddb6006551f88b7cd6d1a2979557076bb9))
* **core:** opt-in chunked SessionWriter (SMELTR_SESSION_INDEX) with seal-on-flush ([a01605c](https://github.com/dgrauet/smeltr/commit/a01605cc5e5bd02133306070a3f3958e59403ec8))
* **core:** pin Source discriminants with repr(u8) + as_u8 for the chunk bitmap ([c764cbb](https://github.com/dgrauet/smeltr/commit/c764cbb314419a0dc184324b891d13f8a0bfdd7b))
* **core:** shared EventFilter (source/time/payload_kind) + chunk_overlaps ([73a2121](https://github.com/dgrauet/smeltr/commit/73a2121f448f136e942dd496e0613ab40ce95a3a))

## [0.11.0](https://github.com/dgrauet/smeltr/compare/v0.10.0...v0.11.0) (2026-06-23)


### Features

* **analyzer:** apply_op_group_by for per-node kind regrouping of the breakdown tree ([1770ce5](https://github.com/dgrauet/smeltr/commit/1770ce5e14df0417cc915cbe0413c0a85fdf64ce))
* group_by="kind" on the inference breakdown tree ([e438a7b](https://github.com/dgrauet/smeltr/commit/e438a7bbece4933030b064577c2ca4f3de78a2ea))
* **mcp,cli:** group_by=kind on the inference breakdown tree ([00c546b](https://github.com/dgrauet/smeltr/commit/00c546baa5bc888f4396b1b2e61f9cc166ddc443))
* **tui:** [/] key filters the Notices feed ([048f9ec](https://github.com/dgrauet/smeltr/commit/048f9ec99b93b17beb9a520be1deb88d6db5dbb2))
* **tui:** [s] key snapshots UiState to JSON ([c483b1f](https://github.com/dgrauet/smeltr/commit/c483b1fb3c353bf75b4a6d4323fc8f27283a9710))
* **tui:** / key filters the Notices feed (input mode) ([0853003](https://github.com/dgrauet/smeltr/commit/08530033f6eb57eed03d7622a86b15d464aaef61))
* **tui:** RenderOverlay + matches_filter for Notices filtering ([4812656](https://github.com/dgrauet/smeltr/commit/4812656580945730f103331da6941c0af8052db3))
* **tui:** s key snapshots UiState to JSON with status feedback ([ff8ca13](https://github.com/dgrauet/smeltr/commit/ff8ca13d6958a6bf6a31b3a1363fdfbbf027adbc))
* **tui:** UiState snapshot-to-JSON helpers ([2f906cc](https://github.com/dgrauet/smeltr/commit/2f906ccb3a754dfe6a94eb49603ed3af8052fdcc))


### Bug Fixes

* **tests:** spawn bins via CARGO_BIN_EXE_* instead of nested cargo build ([a0e2e96](https://github.com/dgrauet/smeltr/commit/a0e2e968c5f5c34efeb6c3c14eb14adc4af2fb07))
* **tests:** use CARGO_BIN_EXE_* instead of nested cargo build (fixes workspace test hang) ([8c9f612](https://github.com/dgrauet/smeltr/commit/8c9f612503e06d2417165777f18172e345cea5c0))

## [0.10.0](https://github.com/dgrauet/smeltr/compare/v0.9.0...v0.10.0) (2026-06-22)


### Features

* **analyzer:** chrome-trace counters for system metrics ([18e8852](https://github.com/dgrauet/smeltr/commit/18e88520ea152388a26a3cc4ad673e06acc4ebbd))
* **analyzer:** chrome-trace counters for VM/thermal/IOReport system metrics ([0768a2c](https://github.com/dgrauet/smeltr/commit/0768a2c92aed128e79bfc8982accdff5ecb5d51a))
* **analyzer:** shared aggregate_ops_flat with group-by name|kind ([6d1d84d](https://github.com/dgrauet/smeltr/commit/6d1d84dec96041424f19f12abae4f3f279458152))
* group_by="kind" for op summary (flat GPU ops) ([ce2e291](https://github.com/dgrauet/smeltr/commit/ce2e2913869ddcff669014b509536b0b89cd49b0))
* **mcp,cli:** group_by name|kind on op summary ([7439891](https://github.com/dgrauet/smeltr/commit/74398915fd3410bf3bb532a7b009f5e811f631de))


### Bug Fixes

* **mcp,analyzer:** clarify op_summary description + cover render kind mode ([e43c175](https://github.com/dgrauet/smeltr/commit/e43c17503f05e4e63c370293547e91f33726380b))

## [0.9.0](https://github.com/dgrauet/smeltr/compare/v0.8.0...v0.9.0) (2026-06-22)


### Features

* **analyzer:** memory-pressure rule + gpu_memory chrome-trace counter ([7ed4f41](https://github.com/dgrauet/smeltr/commit/7ed4f418abc15c3b58921099468341032853895f))
* **analyzer:** memory-pressure rule + gpu_memory counter ([35c93d8](https://github.com/dgrauet/smeltr/commit/35c93d8564639b2edc699ef37911c3cb8c8a0346))

## [0.8.0](https://github.com/dgrauet/smeltr/compare/v0.7.0...v0.8.0) (2026-06-20)


### Features

* **cli:** smeltr tail — NDJSON bus stream with per-session gap detection ([5d97453](https://github.com/dgrauet/smeltr/commit/5d97453830dc813d06b56b331c65baaeb693d265))
* **cli:** smeltr tail — real-time NDJSON event stream (Approach B) ([7550260](https://github.com/dgrauet/smeltr/commit/7550260dbc2cebae358784d579f92c26f68c5f83))
* **daemon:** reusable subscribe_events bus client ([dff4b57](https://github.com/dgrauet/smeltr/commit/dff4b57640d224fec85d3baab4a7fe2c4d26701c))
* **mcp:** subscribe_live delta core (cursor mechanics + payload counts) ([f5f01c0](https://github.com/dgrauet/smeltr/commit/f5f01c026d95547b7c1e80d5cfec7b085177d4a3))
* **mcp:** subscribe_live GPU/top-ops/memory/model-load rollups ([c5fed21](https://github.com/dgrauet/smeltr/commit/c5fed21e8488e6d6b4fd2de2c36195d3e2de3563))
* **mcp:** subscribe_live I/O shell + live-session resolver ([f775417](https://github.com/dgrauet/smeltr/commit/f775417b26c64e90b7ceea5f80f6e63bd72238d1))
* **mcp:** subscribe_live live-tail delta tool ([3a9a543](https://github.com/dgrauet/smeltr/commit/3a9a543753aac46f89c3bc3c53081d0e741a2c8c))
* **mcp:** wire subscribe_live into dispatch and list_tools ([a641eab](https://github.com/dgrauet/smeltr/commit/a641eab2a7e7edeef619d62939607b735bef869e))


### Bug Fixes

* **cli:** smeltr tail SeqTracker ignores seq regression without false gap ([9b8ac8b](https://github.com/dgrauet/smeltr/commit/9b8ac8b9199d5ae7a296a86b8e16316bcf853238))
* **mcp:** return full-UUID session_id and resolve it in resolve_session ([65b2789](https://github.com/dgrauet/smeltr/commit/65b2789b4437802abe24599ee94c99afedd57445))
* **mcp:** subscribe_live memory must scan full history, not events[..cursor] ([c381d47](https://github.com/dgrauet/smeltr/commit/c381d47f496a3641e58619e37679292e7e018845))

## [0.7.0](https://github.com/dgrauet/smeltr/compare/v0.6.0...v0.7.0) (2026-05-21)


### Features

* ModelUnload tracking, per-model stacked area, and analyzer refinement ([#62](https://github.com/dgrauet/smeltr/issues/62)) ([f8b8510](https://github.com/dgrauet/smeltr/commit/f8b8510e2ac3e6c9bd8b2f48e94e4b9f95e979e1))

## [0.6.0](https://github.com/dgrauet/smeltr/compare/v0.5.0...v0.6.0) (2026-05-20)


### Features

* chrome-trace counters + MCP get_model_loads tool (PR3/3) ([#60](https://github.com/dgrauet/smeltr/issues/60)) ([4e4fb98](https://github.com/dgrauet/smeltr/commit/4e4fb98c8aa5016019785e08eeac7d4d9c2fd1b2))
* ModelLoad event + sidecar + duplicate-load analyzer (PR1/3) ([#57](https://github.com/dgrauet/smeltr/issues/57)) ([855782a](https://github.com/dgrauet/smeltr/commit/855782a7ebfc13a6ba02bca8b5b1fb421d89089d))
* **tui:** Models view with cumulative GPU memory + stable colors (PR2/3) ([#59](https://github.com/dgrauet/smeltr/issues/59)) ([7e1e395](https://github.com/dgrauet/smeltr/commit/7e1e39528d4250c83add37cf7dc2d5d0b67a96a1))

## [0.5.0](https://github.com/dgrauet/smeltr/compare/v0.4.1...v0.5.0) (2026-05-20)


### Features

* **mcp,cli:** field-filter for get_inference_breakdown and smeltr breakdown ([#50](https://github.com/dgrauet/smeltr/issues/50)) ([34a6b42](https://github.com/dgrauet/smeltr/commit/34a6b42cd1fa8b0608f51a8347d9f78a84630e79))
* structured fields on smeltr.mark (replace JSON-in-label) ([#52](https://github.com/dgrauet/smeltr/issues/52)) ([a38921a](https://github.com/dgrauet/smeltr/commit/a38921a4e8019fbd863f0cd1919a8ea70b3d891b))
* **tui:** surface scope fields + symbol/kind in breakdown view ([#53](https://github.com/dgrauet/smeltr/issues/53)) ([51924bc](https://github.com/dgrauet/smeltr/commit/51924bc45f9fc21c0c54f15ee8bf2ab2cc3dadd8))

## [0.4.1](https://github.com/dgrauet/smeltr/compare/v0.4.0...v0.4.1) (2026-05-20)


### Bug Fixes

* surface scope **fields end-to-end + per-scope mem samples ([#46](https://github.com/dgrauet/smeltr/issues/46) [#47](https://github.com/dgrauet/smeltr/issues/47)) ([#48](https://github.com/dgrauet/smeltr/issues/48)) ([6226744](https://github.com/dgrauet/smeltr/commit/622674451080c4feedfaa5d591d527e8ae498c0e))

## [0.4.0](https://github.com/dgrauet/smeltr/compare/v0.3.4...v0.4.0) (2026-05-20)


### Features

* scope(name, **fields) structured metadata ([#43](https://github.com/dgrauet/smeltr/issues/43)) ([#44](https://github.com/dgrauet/smeltr/issues/44)) ([e18e2a1](https://github.com/dgrauet/smeltr/commit/e18e2a15db29c3c2642d2772809eda25ab0e2909))

## [0.3.4](https://github.com/dgrauet/smeltr/compare/v0.3.3...v0.3.4) (2026-05-20)


### Bug Fixes

* **analyzer:** 500ms async-grace for dispatch_origins and memory ([#38](https://github.com/dgrauet/smeltr/issues/38) [#40](https://github.com/dgrauet/smeltr/issues/40)) ([#41](https://github.com/dgrauet/smeltr/issues/41)) ([947ee99](https://github.com/dgrauet/smeltr/commit/947ee99dda3cd9a133aec23d3d8203510a4ddaca))

## [0.3.3](https://github.com/dgrauet/smeltr/compare/v0.3.2...v0.3.3) (2026-05-19)


### Bug Fixes

* name propagation for scoped sessions ([#31](https://github.com/dgrauet/smeltr/issues/31) Gap 3) ([#36](https://github.com/dgrauet/smeltr/issues/36)) ([6212fca](https://github.com/dgrauet/smeltr/commit/6212fca59a865068fed10b66f910b4d1b60d91cd))

## [0.3.2](https://github.com/dgrauet/smeltr/compare/v0.3.1...v0.3.2) (2026-05-19)


### Bug Fixes

* scope-token routing for [#31](https://github.com/dgrauet/smeltr/issues/31) Gap 1 ([#34](https://github.com/dgrauet/smeltr/issues/34)) ([bd408c5](https://github.com/dgrauet/smeltr/commit/bd408c544cfb89d1dd9e9570ab0a88f10791efca))

## [0.3.1](https://github.com/dgrauet/smeltr/compare/v0.3.0...v0.3.1) (2026-05-19)


### Bug Fixes

* **release-please:** switch to simple type to handle Cargo workspace inheritance ([#30](https://github.com/dgrauet/smeltr/issues/30)) ([5d2b32c](https://github.com/dgrauet/smeltr/commit/5d2b32cd29cdf6883e41639a9cfe1a045ff5ed5d))

## [Unreleased]

## [0.3.0] - 2026-05-19

Closes #19 — Python-driven profiling enablement. All 7 gaps from the original issue shipped.

### Added

- **Python scopes** (#20) — `smeltr.scope("name")` context manager + decorator for semantic GPU-time attribution. Piggy-backs on existing `mlx.nn.Module` plumbing; zero Rust-side changes.
- **Symbolic kernel names** (#21) — `OpSample.symbol` (e.g. `gemm_t_n_bf16_64_64_32`) captured at PSO creation via `MTLDevice newComputePipelineStateWithFunction:`; analyzer `op_kinds::resolve_kind` maps to canonical names (`Matmul`, `ScaledDotProductAttention`, …).
- **Session naming** (#22) — `SMELTR_SESSION_NAME` env var + `smeltr record --name <NAME>` CLI flag; accepted as an alias by every CLI/MCP session arg (most-recent-wins on collision).
- **Structured export** (#23) — `smeltr.export()` (Python), `smeltr export` (CLI), `export_session` (MCP) producing chrome-trace JSON (chrome://tracing / Perfetto / Speedscope) or raw JSON. 3 swimlanes: Python scopes, Metal CBs, Kernels.
- **Semantic compare diff** (#24) — `compare_sessions` MCP tool + `smeltr compare` CLI surface scope deltas, op-kind deltas, scopes-only-in-A/B.
- **Metal memory tracking** (#25) — `MTLDevice.currentAllocatedSize` sampled at every CB committed/completed; new `Payload::MetalDeviceMemSample`, analyzer `memory.rs`, `get_memory_breakdown` MCP tool, `smeltr memory` CLI, `memory_deltas` in `compare_sessions`. Useful for debugging watchdog OOMs.
- **Python dispatch origins** (#26) — `SMELTR_STACK_CAPTURE=1` opt-in; captures top 3 non-smeltr Python frames at each `mx.eval`. New `MlxEvalEntered.stack_frames`, analyzer `dispatch_origins.rs`, `get_dispatch_origins` MCP tool, `smeltr origins` CLI, `origin_deltas` in `compare_sessions`. Reveals "this Matmul came from `attention.py:127`".

### Documentation

- `CLAUDE.md` (#27): patterns rediscovered across the 7 PRs (additive serde, ring wire format coordination, session resolver, MCP/CLI registration patterns, CI gotchas).
- `docs/usage.md` (#28): per-feature subsections + MCP tool catalog + worked agent workflow.
- MCP server `with_instructions` (#28): expanded from one sentence to a full tool taxonomy.

### Internal

- Ring `RING_VERSION` bumped 1 → 2 (#21 symbolic kernel names) → 3 (#25 device memory samples).
- `compare_sessions` `Response` now has 4 additive vec fields (`scope_deltas`, `op_deltas`, `memory_deltas`, `origin_deltas`) — backward-compat via `#[serde(default, skip_serializing_if = "Vec::is_empty")]`.

## [0.2.0] - 2026-05-17

### Added

- **Per-module GPU time breakdown** for MLX inference (`smeltr breakdown
  [<id>|--last]`). Python sidecar emits `ModuleEntered`/`ModuleReturned`
  events around every `mlx.nn.Module.__call__`; `MlxEvalEntered` now
  carries a `module_stack` snapshot of active call ids. Analyzer
  correlates command-buffer `in_flight_ns` to the leaf module on the
  active stack at MLX eval time.
- **Per-op GPU attribution under each module leaf**, surfaced as
  indented `└ op:K_<pso>_<tg>` lines in the breakdown table.
  Kernels are identified by their `MTLComputePipelineState` pointer
  plus threadgroup-dim signature (MLX does not emit `pushDebugGroup`
  consistently, so semantic names like "Matmul" are not recoverable
  this way). Per-encoder timing via
  `MTLCounterSamplingPointAtStageBoundary` when supported, with a
  pro-rata fallback over the CB's `in_flight_ns` otherwise. Time within
  an encoder is split pro-rata by dispatch count.
- **`smeltr breakdown` flags**: `--top-ops N` (default 5),
  `--no-ops`, `--ops-flat`, plus `--flamegraph <out.svg>` (folded-stack
  flamegraph via the `inferno` crate) and `--chrome-trace <out.json>`
  (Chrome Trace Event Format, viewable in Perfetto and Speedscope).
- **Scoped sessions** per `smeltr record` invocation. The daemon's
  ambient session stays running across all processes; each `smeltr
  record` opens its own session keyed by child PID and routes only
  that process's events into it. `smeltr breakdown --last` defaults
  to the most-recent scoped session; `--include-ambient` opts back to
  the legacy behavior.
- **Python auto-attach** for `smeltr record`. A `.pth` file in
  site-packages triggers `smeltr.attach()` + `decorate_eval()` on
  `import smeltr` when `SMELTR_AUTOLOAD=1` is set (which `smeltr
  record` sets for the child). Unrelated Python processes (pytest,
  notebooks) are unaffected.
- **TUI Notices panel** (renamed from "Log feed"). Surfaces incidents
  (`MetalCbWarning`, `MlxPanicTriggered`), probe-health degradations,
  `smeltr mark`s, and crash-report emissions in one place. Breakdown
  view also gains an op side panel on the selected module (toggled by
  `O`).
- **MCP**: new `get_inference_breakdown` tool returning the
  `ModuleBreakdown` tree with `include_ops` / `top_ops_per_leaf`
  filters; new `get_op_summary` tool for flat cross-module aggregation
  by kernel signature.
- **Kill switches**: `SMELTR_HOOK_NO_OPS=1` disables op-level capture
  in the metal-hook (CB-level capture stays active); the existing
  `SMELTR_HOOK_DISABLE=1` continues to disable the hook entirely.
- **Periodic ticks→ns recalibration** opt-in via
  `SMELTR_HOOK_RECALIBRATE_SEC=<n>`. Re-samples the device CPU/GPU
  tick ratio every N seconds and updates via EMA (alpha=0.2). Useful
  on multi-hour sessions where thermal drift moves the ratio.
  Sanity-rejected samples emit a throttled `MetalHookSkipped`
  diagnostic. Off by default.
- **Per-dispatch GPU timing on M3+** opt-in via
  `SMELTR_HOOK_DISPATCH_BOUNDARY=1` (when the device exposes
  `MTLCounterSamplingPointAtDispatchBoundary`). Replaces the
  encoder-level + pro-rata-within-encoder attribution with exact
  per-dispatch ns. Auto-falls-back to stage-boundary on M1/M2 or on
  sustained sample-buffer alloc failure.
- **MTL4 ML encoder visibility** opt-in via `SMELTR_HOOK_ML_ENCODER=1`
  (macOS 26). Swizzles only `dispatchNetworkWithIntermediatesHeap:`
  on `_MTL4MachineLearningCommandEncoder` (and Debug/Tools variants);
  `setPipelineState:` is deliberately not touched. Dispatches show up
  as `K_MLNet_<encoder_addr>` in the op breakdown.
- **Live hot-kernels TUI panel**: press `K` in `smeltr tui` to toggle
  a top-5 rolling-window (30s) panel that aggregates `MetalCbOps` by
  signature and sorts by `gpu_ns`. Off by default; not part of the
  Tab focus cycle; no layout impact when hidden.

### Changed

- Metal-hook constructor eagerly swizzles the three concrete encoder
  classes MLX instantiates on macOS 14/26 + Apple Silicon
  (`AGXG14XFamilyComputeContext`,
  `AGXG14XFamilyComputeContext_mtlnext`, `_MTL4ComputeCommandEncoder`)
  to track dispatch / pipeline state per encoder. Debug/Tools/ML
  encoder classes are deliberately not swizzled — they crash on
  method-IMP replacement.
- Analyzer applies a 500 ms async-grace window on `MlxEvalReturned`
  intervals so command buffers committed after the MLX eval returns
  (lazy materialization) still attribute to the originating eval call.

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

[Unreleased]: https://github.com/dgrauet/smeltr/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/dgrauet/smeltr/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/dgrauet/smeltr/releases/tag/v0.1.0

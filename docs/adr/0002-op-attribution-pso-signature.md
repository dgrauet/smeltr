# ADR-0002 — Op-level GPU attribution: PSO + threadgroup-dim signature

**Date:** 2026-05-16
**Status:** Accepted

## Context

After Phase 1 (`smeltr breakdown` at module granularity) landed, the next
question was: where inside a module is the GPU time actually going? The
obvious target was decomposing each module's GPU time per MLX
primitive — Matmul, Softmax, LayerNorm, RMSNorm, etc.

The first design (Phase 2) made two assumptions that turned out to be
false in practice:

1. **MLX emits `pushDebugGroup:` / `popDebugGroup` around each primitive's
   Metal dispatches.** This was based on grepping the MLX binary for the
   selector names. They are present, but they are pre-cached selector
   references registered by metal-cpp at init time; MLX 0.31's actual
   kernel encoding path on macOS 14/26 invokes neither selector. Probe
   instrumentation on a 20-iteration `Linear+softmax` workload observed
   **zero** push/pop calls.

2. **The device supports `MTLCounterSamplingPointAtDispatchBoundary`.**
   This is the granularity needed to bracket an individual dispatch
   between two GPU timestamps. The capability is not advertised on
   AGXG14SDevice (M2 Pro): only `AtStageBoundary` is exposed there.

Together these blockers meant the Phase-2-as-designed pipeline (debug
groups + per-dispatch timing) would have emitted zero useful events on
the target hardware/MLX combination.

## Decision

Capture each compute dispatch's **`MTLComputePipelineState` pointer +
threadgroup-dim tuple** as its identity, and time at the **encoder**
boundary instead of the dispatch boundary.

Concretely:

- The metal-hook swizzles `setComputePipelineState:` and the dispatch
  selectors on the concrete compute-encoder classes Apple ships
  (`AGXG14XFamilyComputeContext`, `AGXG14XFamilyComputeContext_mtlnext`,
  `_MTL4ComputeCommandEncoder`). On each dispatch it records `(pso_ptr,
  tg.w, tg.h, tg.d)` in a per-encoder list.
- When the device supports `MTLCounterSamplingPointAtStageBoundary`,
  the hook substitutes MLX's `computeCommandEncoderWithDispatchType:`
  call internally with `computeCommandEncoderWithDescriptor:` carrying
  a synthesized `MTLComputePassDescriptor` whose
  `sampleBufferAttachments[0]` points at a per-encoder
  `MTLCounterSampleBuffer` with `startOfEncoderSampleIndex=0` /
  `endOfEncoderSampleIndex=1`.
- At command-buffer completion, the hook reads each encoder's two
  timestamps for its true GPU duration, then distributes that duration
  pro-rata by dispatch count across the `(pso, tg)` buckets that ran
  inside the encoder. The aggregated buckets are emitted as one
  `MetalCbOps { cb_id, ops: Vec<OpSample> }` per CB.
- When stage sampling is unavailable, or the per-process
  `MTLCounterSampleBuffer` quota is exhausted under load, the hook
  falls back to distributing the CB's existing `in_flight_ns` pro-rata
  by total dispatch count across all encoders in the CB. The wire
  format and the analyzer/CLI/MCP/TUI surface are identical between
  the two timing modes.
- Kernel names are synthesized as `K_<pso_short>_<tg_w>x<tg_h>x<tg_d>`,
  with `pso_short` being the bottom 16 bits of the pointer (cheap to
  hash, stable for the lifetime of the process). `tg=0x0x0` is a
  sentinel for indirect dispatch where the threadgroup count is
  computed by the GPU at runtime.

## Consequences

**Wins:**

- Works on every macOS-14+ Apple Silicon device, not just future M3+
  models that may expose `AtDispatchBoundary`.
- Works with MLX as-shipped, no upstream cooperation required.
- Wire format (`Payload::MetalCbOps`) and analyzer/CLI/MCP/TUI code
  stay identical — only the hook's content of the events changes.
- Stage-boundary timing is exact at the encoder level; the only
  remaining approximation is within a single encoder (pro-rata by
  dispatch count assumes uniform per-dispatch cost).
- Failure modes are graceful: alloc-quota exhaustion silently falls
  back to per-CB pro-rata after 16 consecutive failures, emitting a
  single `MetalHookSkipped` event so the degradation is visible in the
  session record.

**Trade-offs:**

- Names are synthetic. Users see `K_3900_128x33x1`, not "Matmul". A
  future MLX upstream patch emitting `pushDebugGroup(primitive_name)`
  could give us semantic names without changing any other layer.
- Per-encoder rather than per-dispatch granularity. On workloads where
  MLX packs many ops into one encoder, the pro-rata-by-count within
  the encoder will smear the time across them. Per-dispatch timing
  requires `MTLCounterSamplingPointAtDispatchBoundary`.
- We substitute MLX's `WithDispatchType:` call internally. This is
  safe because the substituted `WithDescriptor:` carries the same
  `dispatchType` in the descriptor, but it is a non-trivial
  interception.
- The Debug/Tools/ML encoder classes (`MTLDebugComputeCommandEncoder`,
  `MTL4ToolsComputeCommandEncoder`, `_MTL4MachineLearningCommandEncoder`,
  etc.) are deliberately not swizzled. Replacing their dispatch IMPs
  with our wrappers crashes them (Apple's proxy machinery has stronger
  expectations about the method signatures). The cost is missed
  visibility when running under Xcode GPU debugging or Metal ML
  primitives.

## Alternatives considered

1. **Upstream MLX patch to emit debug groups.** Cleanest semantically,
   but requires landing a change in `ml-explore/mlx` and tying smeltr
   to a minimum MLX version. Kept as a follow-up — it would enable
   semantic names without dropping the PSO-signature path.

2. **dlsym/private-API hooking of `mlx::core::Primitive::eval_gpu`.**
   Gives true primitive names but breaks across every MLX release.
   Hostile to maintenance.

3. **Pure CB-level timing (no per-kernel decomposition).** What we had
   from Phase 1. The whole point of Phase 2 was finer granularity, so
   this would have been giving up.

4. **Apple's private function-handle / function-name APIs on
   `MTLComputePipelineState`.** No public surface; the AGX-family
   pipeline state class does not expose `functionName`. Even if
   reachable via SPI, it would not survive across macOS versions.

## References

- PR #2: implementing commits land Phase 2 and the Phase 2.5 pivot
  together. The design notes that drove the pivot live outside the
  repository as local-only artifacts.

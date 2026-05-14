# Dogfood findings — Plan 10 execution

Date: 2026-05-14
Hardware: Apple Silicon M3 (per CLAUDE.md context)
macOS: 26.5 (25F71)
MLX version: 0.31.2
Python: 3.10.20

## Task 0 — Pré-flight

- [x] cargo build --workspace --release: OK
- [x] make -C metal-hook clean all: OK
- [x] python venv + MLX install: OK (after `uv pip install -e '.[mlx,dev]'`)
- Issues: MLX dep was missing from the freshly-recreated venv (only `[dev]` extra had been installed initially). Documented in Task 1 retest.

## Task 1 — No-crash sanity

Initial run (before Plan 11):
- Session captured: yes
- Event counts:
  - MlxEvalEntered/Returned: 5 / 5
  - MlxMemoryPoll: 6+ (5 Hz polling for ~1.5s)
  - **MetalCbCommitted: 0  🔴 CRITICAL**
  - **MetalCbCompleted: 0  🔴 CRITICAL**
  - MetalBufferAlloc: 18
  - MetalHeapAlloc: 1
- Analyzer report has no ROOT CAUSE: yes (expected — no crash)

**Critical bug found:** zero command-buffer lifecycle events. Investigation via Plan 11 Task 1-3b:
- AGX classes (`AGXG14XFamilyCommandQueue` etc.) are loaded LAZILY after the metal-hook constructor runs. Plan 3's swizzles on AGX classes therefore never installed.
- MLX 0.31+ creates command buffers via `_MTLCommandQueue` (parent class).
- The private `commandBufferDidComplete:startTime:completionTime:error:` signature is non-standard on macOS 26 — Apple passes mach_absolute_time uint64 in GP regs x3/x4 instead of doubles in vector regs d0/d1, breaking our wrapper.

**Fix (Plan 11):**
- Swizzle `_MTLCommandQueue.commandBufferWithDescriptor:` + `.commitCommandBuffer:wake:` (parent-class chokepoints).
- Replace private DidComplete swizzle with public `addCompletedHandler:` block registered at commit time.

After Plan 11:
- MlxEvalEntered/Returned: 5 / 5 (preserved)
- MetalCbCommitted: 5 ✅
- MetalCbCompleted: 5 ✅
- MetalBufferAlloc: 18 ✅
- MetalHeapAlloc: 1 ✅

## Task 2 — Stress test

`scripts/dogfood/mlx_stress.py`: 32 batches × 3 matmuls on 4096×4096 float32, no sync between submissions, single `mx.eval(results)` drain at end.

- Outcome: A (clean exit, no kIOGPU error, no GPU stutter)
- rc=0, total time 5.3s
- Event counts:
  - **MetalCbCommitted: 75** (queue depths captured monotonically 1 → 75)
  - **MetalCbCompleted: 75** (100% capture rate via addCompletedHandler:)
  - MetalBufferAlloc: 71
  - MetalHeapAlloc: 1
- Queue depth peak: **75 CBs in-flight**
- Peak `in_flight_ns`: 1826 ms (CBs at the end of the queue waited ~1.8s)
- `smeltr analyze` output:
  - No ROOT CAUSE (no kIOGPU error — DIM=4096 + 32 batches is sub-threshold)
  - TIMING: `mx.eval call_id=1 returned (sync, 4879ms)` ✅

## Task 3 — Gemma repro

Not executed (no Gemma reproducer script available on this hardware in this session).

## Task 4 — TUI live + replay

Not executed (requires interactive raw-mode terminal; subagent can't render).

## Task 5 — MCP via Claude Desktop / Code

Not executed (requires separate MCP client configuration; user-driven).

## Critical bugs found

- [Plan 11, FIXED] **metal-hook missed all CB lifecycle events on MLX 0.31+ / macOS 26.** Root cause: AGX classes loaded after constructor + private DidComplete selector signature mismatch. Resolution: swizzle `_MTLCommandQueue` parent class + use public `addCompletedHandler:` block API.

## UX gaps found

- `PythonSidecarHello mlx=none` was reported even when MLX was installed. Root: `mlx.__version__` removed in 0.30+. Fixed via `importlib.metadata.version("mlx")` (Plan 11 Task 5).
- `mx.metal.get_*_memory is deprecated` warnings on every poll. Root: legacy MLX API. Fixed by preferring `mx.get_*_memory` (Plan 11 Task 5).
- The CLI `sessions show` was showing `cb_id=`, `buf=`, `MetalCbCompleted`, etc. starting at different column positions due to event-line length variation. The previous Plan 8 Task 2 rendering is sane; my one-liners (`awk '{print $6}'`) were brittle against it. Suggestion: a `sessions show --format json` flag for scripted post-processing.

## Analyzer false negatives / missed patterns

- `QueueDepthRule` only fires when a `MetalCbCompleted` has `error_code != None`. Under stress with peak queue depth 75 and `in_flight=1826ms`, the rule produces no finding because no crash. Consider an advisory `QueuePressureRule` that flags peak queue depth >32 or in-flight >1000ms even without a crash — this is the EARLY WARNING pattern smeltr should help with.

## Analyzer false positives

None observed.

## Recommendations for Plan 12+

1. **Advisory analyzer rule** — flag high queue depth / long in-flight even without kIOGPU errors. Helps users see "danger ramp" before a crash.
2. **`MetalCbScheduled` capture** — currently we only emit Committed + Completed. Apple's `addScheduledHandler:` would give us the third state for finer timing analysis.
3. **JSON output for `sessions show`** — easier scripted post-processing.
4. **Larger stress** — increase DIM to 8192 or batches to 128 to try to trigger a real kIOGPU watchdog. Acceptable only with backup ready.
5. **Try a real Gemma crash repro** when the user has the original failing script on hand — that's the final validation of spec §2.2.

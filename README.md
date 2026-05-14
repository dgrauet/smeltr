# smeltr

Metal/MLX observability and watchdog post-mortem for macOS.

See `docs/superpowers/specs/2026-05-13-smeltr-design.md` for the design.

## Probes (Plan 2)

`smeltrd` runs seven probes by default:

| Probe | What it captures |
|---|---|
| `vm` | wired / active / compressed memory, swap, page-out rate |
| `proc` | top-N CPU; flags `ReportCrash` / `diagnosticservicesd` / `UserNotificationCenter` / `spindump` when above threshold |
| `thermal` | `kern.thermalstate` (Nominal/Light/Moderate/Heavy) — unavailable on some Apple Silicon hosts |
| `oslog` | GPU subsystems + kernel "GPU watchdog" messages via `/usr/bin/log stream` |
| `ioreport` | v1 stub — real IOReport residency lands in Plan 3 with the Metal hook |
| `crash-reports` | parses `.ips` files dropped in `~/Library/Logs/DiagnosticReports/` |
| `mach-exceptions` | attached only to children spawned by `smeltr record` (same-UID PIDs) |

Inspect with `smeltr doctor`. Spawn a watched child with `smeltr record <cmd>`.

## Metal hook (Plan 3)

When `smeltr record <cmd>` is invoked (without `--no-hook`):

1. A 16 MiB ring file is created at `$SMELTR_HOME/rings/<uuid>.ring`.
2. `DYLD_INSERT_LIBRARIES` points to `libmetal_hook.dylib` (embedded in the
   `smeltr` binary and extracted to `$TMPDIR` on first use; overridable with
   `SMELTR_DYLIB=/path/to/libmetal_hook.dylib` for dev builds) and
   `SMELTR_RING_PATH=<ring>` is set in the child environment.
3. The dylib swizzles `MTLDevice.newCommandQueue`, `MTLCommandQueue.commandBuffer`,
   `MTLCommandBuffer.commit`, scheduled / completed handlers, and the
   alloc/dealloc paths of `MTLHeap`, `MTLBuffer`, `MTLTexture`.
4. `smeltrd` reads the ring at 100 Hz via `MetalHookProbe` and emits
   `Payload::Metal*` events into the active session: `MetalCbCommitted`,
   `MetalCbScheduled`, `MetalCbCompleted` (with status, error code/domain,
   in_flight_ns), `MetalCbWarning` (CBs in-flight > 5s),
   `MetalHeapAlloc`/`Free`, `MetalBufferAlloc`/`Free`,
   `MetalTextureAlloc`/`Free`.

Hardened binaries (Apple-shipped Python on Sequoia/Sonoma, e.g.
`/usr/bin/python3`) reject `DYLD_INSERT_LIBRARIES` due to SIP. `smeltr record`
detects this via `codesign --display --verbose=2` (looking for the `runtime`
flag) and falls back to no-hook automatically with a stderr warning. Use
`brew install python` to keep the hook active on Python workloads.

Kill switch: `SMELTR_HOOK_DISABLE=1` makes the loaded dylib inert.

The dylib is built and embedded automatically as part of `cargo build`
(via `crates/smeltr-cli/build.rs`, which invokes `make -C metal-hook all`).
End users never need to touch it. To rebuild the dylib alone during
development: `make -C metal-hook clean all` — output at
`metal-hook/build/libmetal_hook.dylib` (ad-hoc signed). Set
`SMELTR_SKIP_DYLIB_BUILD=1` to skip the make invocation from `build.rs`
(useful for cross-compile / CI with a pre-built dylib).

See `docs/usage.md` for the user-facing usage guide.

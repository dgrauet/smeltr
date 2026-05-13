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

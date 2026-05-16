# smeltr — documentation

This directory holds the in-repo documentation. Planning artifacts
(specs and implementation plans) live outside the repository — they
are local-only design notes intentionally kept out of the project
tree.

## Contents

- [`usage.md`](usage.md) — mental model + recipes for day-to-day use.
- [`adr/`](adr/) — Architecture Decision Records.
  - [`0001-initial-architecture.md`](adr/0001-initial-architecture.md)
    — initial architecture (daemon + probes + hook + sidecars + flight
    recorder + analyzer + TUI + MCP).
  - [`0002-op-attribution-pso-signature.md`](adr/0002-op-attribution-pso-signature.md)
    — op-level GPU attribution: PSO + threadgroup-dim signature
    instead of MLX debug groups, stage-boundary timing instead of
    dispatch-boundary.

## See also

- The top-level [README.md](../README.md) for an overview and quick start.
- [CLAUDE.md](../CLAUDE.md) for working conventions and developer notes.
- [CHANGELOG.md](../CHANGELOG.md) for release history.

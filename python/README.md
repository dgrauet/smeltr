# smeltr (Python sidecar)

Opt-in Python companion to the `smeltr` Metal/MLX observability daemon.

Connects to a running `smeltrd` via Unix socket and emits semantic markers,
MLX eval tracing, MLX memory polling, and live-array tracking.

See the parent project README for the full picture.

## Install

Not published on PyPI — install from the smeltr clone, **in each
environment your workloads run in** (every venv separately):

```
pip install -e python/                # from the smeltr repo
pip install -e 'python/[mlx]'         # with mlx integration
```

## Auto-attach

The package installs a `smeltr-autoload.pth` into `site-packages`; at
interpreter startup it imports `smeltr._autoload` only when
`SMELTR_AUTOLOAD=1` is in the environment (otherwise it imports nothing).
`smeltr record` sets that variable in the child it spawns, so code run
under `smeltr record` is observed with zero modification. Any other Python
invocation is untouched. The sidecar never imports `mlx` itself: MLX
(`mx.eval`, `nn.Module`, memory polling, model loads) is instrumented when
the program imports it, so a launcher or helper that never uses MLX does
not load the Metal backend.

Events are queued and sent by a background thread: an emit never waits for
the daemon. If the daemon is stalled or gone, events beyond the queue bound
are dropped and counted, and the count lands in the session as a
`smeltr: sidecar dropped events` mark.
Call `smeltr.attach()` manually only for processes not launched via
`smeltr record` (always-on mode).

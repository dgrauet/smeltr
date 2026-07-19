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
interpreter startup it imports `smeltr._autoload`, which attaches only
when `SMELTR_AUTOLOAD=1` is in the environment. `smeltr record` sets that
variable in the child it spawns, so code run under `smeltr record` is
observed with zero modification. Any other Python invocation is untouched.
Call `smeltr.attach()` manually only for processes not launched via
`smeltr record` (always-on mode).

# smeltr (Python sidecar)

Opt-in Python companion to the `smeltr` Metal/MLX observability daemon.

Connects to a running `smeltrd` via Unix socket and emits semantic markers,
MLX eval tracing, MLX memory polling, and live-array tracking.

See the parent project README for the full picture.

## Install

```
pip install -e python/                # from the smeltr repo
pip install -e 'python/[mlx]'         # with mlx integration
```

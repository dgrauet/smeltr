"""Minimal MLX workload for dogfood sanity check.

Allocates a few arrays, runs a couple of evals, exits cleanly. Used to
verify that the smeltr capture pipeline produces a session with
MlxEvalEntered/Returned, MlxMemoryPoll, MetalCbCommitted/Completed events.
"""
import time

import smeltr
import mlx.core as mx

smeltr.attach(poll_hz=5.0)
smeltr.decorate_eval()

with smeltr.session("mlx-no-crash-sanity"):
    smeltr.mark("phase: alloc")
    a = mx.random.normal((1024, 1024))
    b = mx.random.normal((1024, 1024))
    smeltr.mark("phase: matmul")
    for i in range(5):
        c = mx.matmul(a, b)
        mx.eval(c)
        smeltr.mark(f"iteration {i}")
        time.sleep(0.2)
    smeltr.mark("phase: done")

smeltr.detach()
print("smeltr no-crash sanity finished cleanly.")

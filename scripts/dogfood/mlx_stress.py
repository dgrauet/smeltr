"""Pushes MLX to a high CB queue depth via many submissions without
synchronisation between them.

USAGE WARNING: this script enqueues large GPU work. Close other GPU apps
before running. If you see a system stutter, Ctrl-C immediately.
"""
import smeltr
import mlx.core as mx

smeltr.attach()
smeltr.decorate_eval()

DIM = 4096

with smeltr.session("mlx-stress"):
    smeltr.mark("phase: alloc")
    a = mx.random.normal((DIM, DIM))
    b = mx.random.normal((DIM, DIM))

    smeltr.mark("phase: submit-flood")
    results = []
    for i in range(32):
        c = mx.matmul(a, b)
        c = mx.matmul(c, a)
        c = mx.matmul(c, b)
        results.append(c)
        smeltr.mark(f"submitted batch {i}")

    smeltr.mark("phase: drain")
    mx.eval(results)
    smeltr.mark("phase: drained")

smeltr.detach()
print("smeltr stress finished.")

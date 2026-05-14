import gc
import time

import smeltr
from smeltr import _mlx


class _FakeArray:
    """Stand-in for mx.array in unit tests. Weakref-able."""

    def __init__(self, size: int, dtype_name: str, shape: tuple):
        class _DType:
            name = dtype_name
            itemsize = 1
        self.size = size
        self.dtype = _DType()
        self.shape = shape


def test_track_emits_array_alive(fake_daemon):
    smeltr.attach(poll_hz=0)
    try:
        a = _FakeArray(64, "float32", (4, 4))
        _mlx.track(a, stream="gpu")
        alives = [m for m in fake_daemon.received
                  if m["payload"]["kind"] == "MlxArrayAlive"]
        assert len(alives) == 1
        p = alives[0]["payload"]
        assert p["size_bytes"] == 64
        assert p["dtype"] == "float32"
        assert p["shape"] == [4, 4]
        assert p["stream"] == "gpu"
    finally:
        smeltr.detach()


def test_array_freed_on_gc(fake_daemon):
    smeltr.attach(poll_hz=0)
    try:
        a = _FakeArray(128, "float16", (2, 8, 8))
        _mlx.track(a, stream="gpu")
        del a
        gc.collect()
        time.sleep(0.05)
        freed = [m for m in fake_daemon.received
                 if m["payload"]["kind"] == "MlxArrayFreed"]
        assert len(freed) == 1
    finally:
        smeltr.detach()


def test_snapshot_emits_summary(fake_daemon):
    smeltr.attach(poll_hz=0)
    try:
        a = _FakeArray(64, "float32", (4, 4))
        b = _FakeArray(128, "float16", (8, 8))
        _mlx.track(a, stream="gpu")
        _mlx.track(b, stream="gpu")
        smeltr.snapshot()
    finally:
        smeltr.detach()
    snaps = [m for m in fake_daemon.received
             if m["payload"]["kind"] == "MlxSnapshot"]
    assert len(snaps) == 1
    p = snaps[0]["payload"]
    assert p["live_arrays"] == 2
    assert p["total_array_bytes"] == 64 + 128
    assert "gpu" in p["streams"]


def test_snapshot_skips_when_not_attached():
    smeltr.snapshot()  # no-op, no exception

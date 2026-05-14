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


def test_polling_emits_memory_poll(monkeypatch, fake_daemon):
    from smeltr import _mlx as _mlxmod

    fake_module = type("FakeMxMetal", (), {
        "get_active_memory": staticmethod(lambda: 1024),
        "get_peak_memory":   staticmethod(lambda: 2048),
        "get_cache_memory":  staticmethod(lambda: 512),
    })()
    monkeypatch.setattr(_mlxmod, "_get_mx_metal", lambda: fake_module)

    smeltr.attach(poll_hz=20.0)
    try:
        time.sleep(0.25)
    finally:
        smeltr.detach()

    polls = [m for m in fake_daemon.received
             if m["payload"]["kind"] == "MlxMemoryPoll"]
    assert len(polls) >= 2
    assert polls[0]["payload"]["active_bytes"] == 1024
    assert polls[0]["payload"]["peak_bytes"] == 2048
    assert polls[0]["payload"]["cache_bytes"] == 512


def test_polling_disabled_when_poll_hz_zero(fake_daemon, monkeypatch):
    from smeltr import _mlx as _mlxmod
    fake_module = type("FakeMxMetal", (), {
        "get_active_memory": staticmethod(lambda: 1),
        "get_peak_memory":   staticmethod(lambda: 1),
        "get_cache_memory":  staticmethod(lambda: 1),
    })()
    monkeypatch.setattr(_mlxmod, "_get_mx_metal", lambda: fake_module)

    smeltr.attach(poll_hz=0)
    try:
        time.sleep(0.1)
    finally:
        smeltr.detach()
    polls = [m for m in fake_daemon.received
             if m["payload"]["kind"] == "MlxMemoryPoll"]
    assert polls == []


def test_polling_skipped_when_mlx_absent(fake_daemon, monkeypatch):
    from smeltr import _mlx as _mlxmod
    monkeypatch.setattr(_mlxmod, "_get_mx_metal", lambda: None)

    smeltr.attach(poll_hz=20.0)
    try:
        time.sleep(0.1)
    finally:
        smeltr.detach()
    polls = [m for m in fake_daemon.received
             if m["payload"]["kind"] == "MlxMemoryPoll"]
    assert polls == []


def test_decorate_eval_emits_enter_and_return(fake_daemon, monkeypatch):
    import types
    import sys as _sys

    fake_core = types.ModuleType("mlx.core")
    call_log = []

    def fake_eval(*args, **kwargs):
        call_log.append(args)
        return None

    fake_core.eval = fake_eval
    fake_root = types.ModuleType("mlx")
    fake_root.core = fake_core
    monkeypatch.setitem(_sys.modules, "mlx", fake_root)
    monkeypatch.setitem(_sys.modules, "mlx.core", fake_core)

    smeltr.attach(poll_hz=0)
    try:
        smeltr.decorate_eval()
        import mlx.core as mx_core
        mx_core.eval("array1", "array2", "array3")
    finally:
        smeltr.detach()

    enters = [m for m in fake_daemon.received
              if m["payload"]["kind"] == "MlxEvalEntered"]
    returns = [m for m in fake_daemon.received
               if m["payload"]["kind"] == "MlxEvalReturned"]
    assert len(enters) == 1
    assert len(returns) == 1
    assert enters[0]["payload"]["array_count"] == 3
    assert returns[0]["payload"]["call_id"] == enters[0]["payload"]["call_id"]
    assert returns[0]["payload"]["duration_ns"] >= 0
    assert call_log == [("array1", "array2", "array3")]


def test_decorate_eval_is_idempotent(fake_daemon, monkeypatch):
    import types
    import sys as _sys
    fake_core = types.ModuleType("mlx.core")
    fake_core.eval = lambda *a, **k: None
    fake_root = types.ModuleType("mlx")
    fake_root.core = fake_core
    monkeypatch.setitem(_sys.modules, "mlx", fake_root)
    monkeypatch.setitem(_sys.modules, "mlx.core", fake_core)

    smeltr.attach(poll_hz=0)
    try:
        smeltr.decorate_eval()
        first_wrap = fake_core.eval
        smeltr.decorate_eval()
        assert fake_core.eval is first_wrap
    finally:
        smeltr.detach()


def test_decorate_eval_noop_without_mlx(fake_daemon, monkeypatch):
    import sys as _sys
    import builtins

    for name in list(_sys.modules):
        if name == "mlx" or name.startswith("mlx."):
            monkeypatch.delitem(_sys.modules, name, raising=False)

    real_import = builtins.__import__

    def blocked_import(name, *a, **k):
        if name == "mlx" or name.startswith("mlx."):
            raise ImportError("mlx not installed (simulated)")
        return real_import(name, *a, **k)

    monkeypatch.setattr(builtins, "__import__", blocked_import)

    smeltr.attach(poll_hz=0)
    try:
        smeltr.decorate_eval()  # must not raise
    finally:
        smeltr.detach()


def test_decorate_eval_was_async_reflects_duration(fake_daemon, monkeypatch):
    """Short calls report was_async=True, long calls was_async=False."""
    import time as _time
    import types
    import sys as _sys

    fake_core = types.ModuleType("mlx.core")

    def slow_fn(*args, **kwargs):
        _time.sleep(0.05)  # 50ms — exceeds 10ms threshold
        return None

    def fast_fn(*args, **kwargs):
        return None  # essentially instant

    fake_root = types.ModuleType("mlx")
    fake_root.core = fake_core
    monkeypatch.setitem(_sys.modules, "mlx", fake_root)
    monkeypatch.setitem(_sys.modules, "mlx.core", fake_core)

    smeltr.attach(poll_hz=0)
    try:
        # First call: fast → should report was_async=True
        fake_core.eval = fast_fn
        smeltr.decorate_eval()
        import mlx.core as mx_core
        mx_core.eval("a1")

        # Undecorate, swap, redecorate, slow call
        from smeltr._mlx import _undecorate_eval_for_tests
        _undecorate_eval_for_tests()
        fake_core.eval = slow_fn
        smeltr.decorate_eval()
        mx_core.eval("a2")
    finally:
        smeltr.detach()

    returns = [m for m in fake_daemon.received
               if m["payload"]["kind"] == "MlxEvalReturned"]
    assert len(returns) == 2
    fast = returns[0]["payload"]
    slow = returns[1]["payload"]
    assert fast["was_async"] is True, f"fast call should be async, got {fast}"
    assert slow["was_async"] is False, f"slow call should be sync, got {slow}"
    assert slow["duration_ns"] >= 50_000_000


def test_snapshot_includes_mlx_streams_when_available(fake_daemon, monkeypatch):
    """When mx.default_stream / mx.cpu_stream / mx.gpu_stream exist,
    their reprs appear in MlxSnapshot.streams (in addition to observed)."""
    import types
    import sys as _sys

    class FakeStream:
        def __init__(self, name):
            self._name = name
        def __repr__(self):
            return f"Stream(device={self._name})"

    fake_core = types.ModuleType("mlx.core")
    fake_core.default_stream = lambda: FakeStream("gpu")
    fake_core.cpu_stream = lambda: FakeStream("cpu")
    fake_core.gpu_stream = lambda: FakeStream("gpu")
    fake_root = types.ModuleType("mlx")
    fake_root.core = fake_core
    monkeypatch.setitem(_sys.modules, "mlx", fake_root)
    monkeypatch.setitem(_sys.modules, "mlx.core", fake_core)

    smeltr.attach(poll_hz=0)
    try:
        smeltr.snapshot()
    finally:
        smeltr.detach()

    snaps = [m for m in fake_daemon.received
             if m["payload"]["kind"] == "MlxSnapshot"]
    assert len(snaps) == 1
    streams = snaps[0]["payload"]["streams"]
    assert any("gpu" in s.lower() for s in streams), f"no gpu stream in {streams}"
    assert any("cpu" in s.lower() for s in streams), f"no cpu stream in {streams}"


def test_snapshot_streams_falls_back_to_observed_when_mlx_missing(fake_daemon, monkeypatch):
    """If mx.core has no stream factories, streams comes from track() only."""
    import types
    import sys as _sys

    fake_core = types.ModuleType("mlx.core")  # No default_stream / cpu_stream / gpu_stream
    fake_root = types.ModuleType("mlx")
    fake_root.core = fake_core
    monkeypatch.setitem(_sys.modules, "mlx", fake_root)
    monkeypatch.setitem(_sys.modules, "mlx.core", fake_core)

    from smeltr import _mlx as _mlxmod

    smeltr.attach(poll_hz=0)
    try:
        class _FakeArr:
            def __init__(self):
                class _DT:
                    name = "float32"
                    itemsize = 4
                self.size = 16
                self.dtype = _DT()
                self.shape = (4, 4)
        a = _FakeArr()
        _mlxmod.track(a, stream="gpu")
        smeltr.snapshot()
    finally:
        smeltr.detach()

    snaps = [m for m in fake_daemon.received
             if m["payload"]["kind"] == "MlxSnapshot"]
    assert len(snaps) == 1
    streams = snaps[0]["payload"]["streams"]
    assert "gpu" in streams

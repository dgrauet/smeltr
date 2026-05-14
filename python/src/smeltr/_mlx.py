"""MLX integration: weakref-based array tracking and snapshot()."""

from __future__ import annotations

import threading
import weakref
from typing import Any

from smeltr._api import _emit, _require_attached, _detect_mlx_version  # type: ignore

# array_id -> (size_bytes, dtype, shape, stream)
_tracked: dict[int, tuple[int, str, list[int], str]] = {}
_tracked_lock = threading.Lock()


def _array_id(obj: Any) -> int:
    return id(obj) & 0xFFFF_FFFF_FFFF_FFFF


def _shape_list(obj: Any) -> list[int]:
    shape = getattr(obj, "shape", ())
    return [int(x) for x in shape]


def _dtype_str(obj: Any) -> str:
    d = getattr(obj, "dtype", "unknown")
    return getattr(d, "name", str(d))


def _size_bytes(obj: Any) -> int:
    size = getattr(obj, "size", 0)
    itemsize = getattr(getattr(obj, "dtype", None), "itemsize", 1)
    try:
        return int(size) * int(itemsize)
    except (TypeError, ValueError):
        return int(size)


def track(array: Any, *, stream: str = "gpu") -> None:
    aid = _array_id(array)
    record = (_size_bytes(array), _dtype_str(array), _shape_list(array), stream)
    with _tracked_lock:
        _tracked[aid] = record
    try:
        weakref.finalize(array, _on_free, aid)
    except TypeError:
        pass
    try:
        _emit({
            "kind": "MlxArrayAlive",
            "array_id": aid,
            "size_bytes": record[0],
            "dtype": record[1],
            "shape": record[2],
            "stream": record[3],
        })
    except Exception:
        pass


def _on_free(aid: int) -> None:
    with _tracked_lock:
        _tracked.pop(aid, None)
    try:
        _emit({"kind": "MlxArrayFreed", "array_id": aid})
    except Exception:
        pass


def snapshot() -> None:
    try:
        _require_attached()
    except RuntimeError:
        return
    with _tracked_lock:
        records = list(_tracked.values())
    total = sum(r[0] for r in records)
    observed_streams = {r[3] for r in records}
    introspected = _introspect_mlx_streams()
    streams = sorted(observed_streams | introspected)
    try:
        _emit({
            "kind": "MlxSnapshot",
            "live_arrays": len(records),
            "total_array_bytes": total,
            "streams": streams,
            "mlx_version": _detect_mlx_version(),
        })
    except Exception:
        pass


def _introspect_mlx_streams() -> set[str]:
    """Returns the names of MLX streams discoverable via mlx.core factories.

    MLX exposes `default_stream()`, and optionally `cpu_stream()` /
    `gpu_stream()` factories on the `mlx.core` module. Each is called and
    the result converted to a label via `repr()`. MLX does not expose
    per-stream queue depth, so this is purely an enumeration of which
    streams exist.

    Returns an empty set if mlx is not importable or none of the factories
    exist.
    """
    try:
        import mlx.core as mx_core
    except ImportError:
        return set()
    out: set[str] = set()
    for factory_name in ("default_stream", "cpu_stream", "gpu_stream"):
        factory = getattr(mx_core, factory_name, None)
        if factory is None:
            continue
        try:
            stream = factory()
        except Exception:
            continue
        out.add(repr(stream))
    return out


def _reset_for_tests() -> None:
    _undecorate_eval_for_tests()
    stop_polling()
    with _tracked_lock:
        _tracked.clear()


# ---- mx.metal polling ----

_poll_thread: threading.Thread | None = None
_poll_stop = threading.Event()


def _get_mx_metal() -> Any | None:
    try:
        import mlx.core as mx_core
        return getattr(mx_core, "metal", None)
    except ImportError:
        return None


def start_polling(poll_hz: float) -> None:
    """Start the background memory poller. Safe to call multiple times."""
    global _poll_thread
    stop_polling()
    if poll_hz <= 0:
        return
    if _get_mx_metal() is None:
        return
    _poll_stop.clear()

    def _loop():
        period = 1.0 / poll_hz
        while not _poll_stop.is_set():
            mx_metal = _get_mx_metal()
            if mx_metal is None:
                return
            try:
                active = int(mx_metal.get_active_memory())
                peak = int(mx_metal.get_peak_memory())
                cache = int(mx_metal.get_cache_memory())
            except Exception:
                _poll_stop.wait(period)
                continue
            try:
                _emit({
                    "kind": "MlxMemoryPoll",
                    "active_bytes": active,
                    "peak_bytes": peak,
                    "cache_bytes": cache,
                })
            except Exception:
                pass
            _poll_stop.wait(period)

    _poll_thread = threading.Thread(target=_loop, daemon=True, name="smeltr-mx-poll")
    _poll_thread.start()


def stop_polling() -> None:
    global _poll_thread
    _poll_stop.set()
    t = _poll_thread
    if t is not None:
        t.join(timeout=2.0)
    _poll_thread = None


# ---- mx.core.eval decoration ----

_eval_decorated = False
_eval_call_counter = 0
_eval_call_counter_lock = threading.Lock()


def _next_call_id() -> int:
    global _eval_call_counter
    with _eval_call_counter_lock:
        _eval_call_counter += 1
        return _eval_call_counter


def decorate_eval() -> None:
    """Monkey-patch mx.core.eval to emit MlxEvalEntered/Returned events.

    No-op if MLX is not importable or if already decorated.

    The `was_async` field on MlxEvalReturned is a heuristic: calls returning
    in less than 10 ms are flagged async (work queued but not awaited),
    longer calls are flagged sync (presumably blocked on a backend sync).
    MLX does not expose a public API to know whether a sync actually
    happened, so this is the best available signal.
    """
    global _eval_decorated
    if _eval_decorated:
        return
    try:
        import mlx.core as mx_core
    except ImportError:
        return
    original = getattr(mx_core, "eval", None)
    if original is None or getattr(original, "_smeltr_wrapped", False):
        return

    import time as _time

    def wrapped(*args, **kwargs):
        call_id = _next_call_id()
        try:
            _emit({
                "kind": "MlxEvalEntered",
                "call_id": call_id,
                "array_count": len(args),
                "stream": "gpu",
            })
        except Exception:
            pass
        start = _time.monotonic_ns()
        try:
            return original(*args, **kwargs)
        finally:
            end = _time.monotonic_ns()
            ASYNC_THRESHOLD_NS = 10_000_000  # 10 ms — empirical: faster returns are almost certainly async
            duration = end - start
            try:
                _emit({
                    "kind": "MlxEvalReturned",
                    "call_id": call_id,
                    "duration_ns": duration,
                    "was_async": duration < ASYNC_THRESHOLD_NS,
                })
            except Exception:
                pass

    wrapped._smeltr_wrapped = True  # type: ignore[attr-defined]
    wrapped._smeltr_original = original  # type: ignore[attr-defined]
    mx_core.eval = wrapped
    _eval_decorated = True


def _undecorate_eval_for_tests() -> None:
    global _eval_decorated
    _eval_decorated = False
    try:
        import mlx.core as mx_core
    except ImportError:
        return
    current = getattr(mx_core, "eval", None)
    if getattr(current, "_smeltr_wrapped", False):
        mx_core.eval = current._smeltr_original

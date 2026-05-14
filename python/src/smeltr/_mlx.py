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
    streams = sorted({r[3] for r in records})
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


def _reset_for_tests() -> None:
    with _tracked_lock:
        _tracked.clear()

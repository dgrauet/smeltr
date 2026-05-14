"""Public API surface: attach, detach, session, mark, now."""

from __future__ import annotations

import contextlib
import json
import platform
import sys
import threading
import time
from typing import Iterator

from smeltr._client import ClientError, _Client
from smeltr._proto import SOURCE_PYTHON_SIDECAR

_client: _Client | None = None
_client_lock = threading.Lock()


def _detect_mlx_version() -> str | None:
    try:
        import mlx
        return getattr(mlx, "__version__", None)
    except ImportError:
        return None


def attach(client_name: str = "smeltr-py", timeout_s: float = 2.0,
           poll_hz: float = 1.0) -> None:
    """Connect to smeltrd. poll_hz is wired up in a later task; accept it now
    so callers don't need to change later."""
    global _client
    with _client_lock:
        if _client is not None:
            _client.close()
        c = _Client(client_name=client_name)
        c.connect(timeout_s=timeout_s)
        _client = c
    try:
        _emit({
            "kind": "PythonSidecarHello",
            "python_version": platform.python_version(),
            "mlx_version": _detect_mlx_version(),
            "argv": list(sys.argv),
        })
    except ClientError:
        pass


def detach() -> None:
    """Close the daemon connection. Idempotent."""
    global _client
    with _client_lock:
        if _client is not None:
            _client.close()
            _client = None


def _require_attached() -> _Client:
    if _client is None:
        raise RuntimeError("smeltr.attach() must be called first")
    return _client


def _emit(payload: dict, *, pid: int | None = None) -> None:
    c = _require_attached()
    c.emit(payload, pid=pid, source=SOURCE_PYTHON_SIDECAR)


def mark(label: str, **fields: object) -> None:
    """Drop a labelled event on the timeline."""
    if fields:
        label = f"{label} {json.dumps(fields, default=str, sort_keys=True)}"
    _emit({"kind": "Mark", "label": label})


def now() -> int:
    """Return a monotonic timestamp in ns aligned with the daemon clock."""
    return time.monotonic_ns()


@contextlib.contextmanager
def session(name: str) -> Iterator[None]:
    _require_attached()
    mark(f"session-open: {name}")
    try:
        yield
    finally:
        mark(f"session-close: {name}")

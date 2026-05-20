"""Model-load tracking: wrap safetensors and mlx.core.load to emit ModelLoad events.

`decorate_model_loads()` is idempotent — calling it multiple times is a no-op.
"""

from __future__ import annotations

import hashlib
import os
import time
from typing import Any

from smeltr._api import _emit

_decorated = False


def _compute_sha8(canonical_path: str) -> str:
    return hashlib.sha256(canonical_path.encode()).hexdigest()[:8]


def _emit_model_load(
    path: Any,
    t_start_ns: int,
    t_end_ns: int,
    framework: str,
) -> None:
    """Emit a ModelLoad event. Silently skips if path is not a str."""
    if not isinstance(path, (str, bytes, os.PathLike)):
        return
    try:
        canonical = os.path.realpath(os.fspath(path))
    except Exception:
        return
    try:
        size_bytes = os.path.getsize(canonical)
    except OSError:
        return
    try:
        sha8 = _compute_sha8(canonical)
        _emit(
            {
                "kind": "ModelLoad",
                "path": canonical,
                "size_bytes": size_bytes,
                "t_start_ns": t_start_ns,
                "t_end_ns": t_end_ns,
                "sha8": sha8,
                "framework": framework,
            }
        )
    except Exception:
        # Observability must never break user code.
        pass


def decorate_model_loads() -> None:
    """Monkey-patch safetensors and mlx.core.load to emit ModelLoad events.

    Idempotent: calling this function more than once is a no-op.
    """
    global _decorated
    if _decorated:
        return
    _decorated = True

    _wrap_safetensors_safe_open()
    _wrap_safetensors_torch_load_file()
    _wrap_mlx_core_load()


def _wrap_safetensors_safe_open() -> None:
    try:
        import safetensors as _st
    except ImportError:
        return

    original = getattr(_st, "safe_open", None)
    if original is None or getattr(original, "_smeltr_wrapped", False):
        return

    def wrapped(filename: Any, *args: Any, **kwargs: Any) -> Any:
        t_start = time.monotonic_ns()
        try:
            result = original(filename, *args, **kwargs)
        finally:
            t_end = time.monotonic_ns()
            _emit_model_load(filename, t_start, t_end, "safetensors")
        return result

    wrapped._smeltr_wrapped = True  # type: ignore[attr-defined]
    wrapped._smeltr_original = original  # type: ignore[attr-defined]
    _st.safe_open = wrapped  # type: ignore[attr-defined]


def _wrap_safetensors_torch_load_file() -> None:
    try:
        import safetensors.torch as _st_torch
    except ImportError:
        return

    original = getattr(_st_torch, "load_file", None)
    if original is None or getattr(original, "_smeltr_wrapped", False):
        return

    def wrapped(filename: Any, *args: Any, **kwargs: Any) -> Any:
        t_start = time.monotonic_ns()
        try:
            result = original(filename, *args, **kwargs)
        finally:
            t_end = time.monotonic_ns()
            _emit_model_load(filename, t_start, t_end, "safetensors")
        return result

    wrapped._smeltr_wrapped = True  # type: ignore[attr-defined]
    wrapped._smeltr_original = original  # type: ignore[attr-defined]
    _st_torch.load_file = wrapped


def _wrap_mlx_core_load() -> None:
    try:
        import mlx.core as _mx_core
    except ImportError:
        return

    original = getattr(_mx_core, "load", None)
    if original is None or getattr(original, "_smeltr_wrapped", False):
        return

    def wrapped(file: Any, *args: Any, **kwargs: Any) -> Any:
        t_start = time.monotonic_ns()
        try:
            result = original(file, *args, **kwargs)
        finally:
            t_end = time.monotonic_ns()
            _emit_model_load(file, t_start, t_end, "mlx")
        return result

    wrapped._smeltr_wrapped = True  # type: ignore[attr-defined]
    wrapped._smeltr_original = original  # type: ignore[attr-defined]
    _mx_core.load = wrapped


def _undecorate_for_tests() -> None:
    """Restore all wrapped functions and reset the decorated flag. Tests only."""
    global _decorated
    _decorated = False

    try:
        import safetensors as _st

        current = getattr(_st, "safe_open", None)
        if current is not None and getattr(current, "_smeltr_wrapped", False):
            _st.safe_open = current._smeltr_original  # type: ignore[attr-defined]
    except ImportError:
        pass

    try:
        import safetensors.torch as _st_torch

        current = getattr(_st_torch, "load_file", None)
        if current is not None and getattr(current, "_smeltr_wrapped", False):
            _st_torch.load_file = current._smeltr_original
    except ImportError:
        pass

    try:
        import mlx.core as _mx_core

        current = getattr(_mx_core, "load", None)
        if current is not None and getattr(current, "_smeltr_wrapped", False):
            _mx_core.load = current._smeltr_original
    except ImportError:
        pass

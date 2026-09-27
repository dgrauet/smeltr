"""Module-call tracking: monkey-patch mlx.nn.Module.__call__ to emit
ModuleEntered/ModuleReturned events and maintain a thread-local stack
that the mx.eval hook can snapshot into MlxEvalEntered.module_stack.
"""

from __future__ import annotations

import functools
import itertools
import os
import threading
from typing import Any

from smeltr._api import _emit as _api_emit
from smeltr._log import warning

_tls = threading.local()
# itertools.count: next() is atomic under the GIL, so ids need no lock — a
# lock another thread holds at fork() stays held in the child forever (#266).
_call_counter = itertools.count(1)
_installed = False
_install_lock = threading.Lock()

# Track which classes have been wrapped so we can uninstall cleanly.
_wrapped_classes: list[type] = []
# Dedicated lock for _wrapped_classes mutations (avoids reentrant deadlock with
# _install_lock, which is held for the full duration of install()).
_wrapped_classes_lock = threading.RLock()


def _emit(payload: dict[str, Any]) -> None:
    """Thin indirection so tests can patch it cleanly."""
    try:
        _api_emit(payload)
    except Exception:
        # Observability must never break the user's code.
        pass


def _next_call_id() -> int:
    return next(_call_counter)


def _stack() -> list[dict[str, Any]]:
    s = getattr(_tls, "stack", None)
    if s is None:
        s = []
        _tls.stack = s
    return s


def _current_stack() -> list[int]:
    return [frame["module_call_id"] for frame in _stack()]


def _push(
    qualname: str,
    class_name: str,
    *,
    id_of: int,
    fields: dict[str, Any] | None = None,
) -> int:
    stack = _stack()
    parent = stack[-1]["module_call_id"] if stack else None
    cid = _next_call_id()
    frame = {
        "module_call_id": cid,
        "module_def_id": id_of & 0xFFFF_FFFF_FFFF_FFFF,
        "qualname": qualname,
        "class_name": class_name,
        "depth": len(stack),
    }
    stack.append(frame)
    payload: dict[str, Any] = {
        "kind": "ModuleEntered",
        "module_call_id": cid,
        "module_def_id": frame["module_def_id"],
        "qualname": qualname,
        "class_name": class_name,
        "parent_call_id": parent,
        "depth": frame["depth"],
    }
    if fields:
        payload["fields"] = _coerce_fields(fields)
    _emit(payload)
    return cid


def _coerce_fields(fields: dict[str, Any]) -> dict[str, Any]:
    """Coerce values to CBOR-friendly primitives (bool/int/float/str).

    Non-primitives are stringified via str() so emit never raises.
    """
    out: dict[str, Any] = {}
    for k, v in fields.items():
        if isinstance(v, (bool, int, float, str)):
            out[k] = v
            continue
        try:
            out[k] = str(v)
        except Exception:
            out[k] = f"<unprintable {type(v).__name__}>"
    return out


def _pop(expected_cid: int) -> None:
    stack = _stack()
    found = False
    if stack and stack[-1]["module_call_id"] == expected_cid:
        stack.pop()
        found = True
    else:
        for i in range(len(stack) - 1, -1, -1):
            if stack[i]["module_call_id"] == expected_cid:
                del stack[i]
                found = True
                break
    if found:
        _emit({"kind": "ModuleReturned", "module_call_id": expected_cid})


def _qualname_for(module: Any) -> str:
    cls = type(module).__name__
    # Only a non-empty str names the instance: `name` is a user attribute
    # like any other and may hold an mx.array, whose bool() raises (#266).
    label = getattr(module, "name", None)
    if isinstance(label, str) and label and label != cls:
        return f"{cls}:{label}"
    return cls


def _enter_module(module: Any) -> int | None:
    """Push a frame for a module call. Returns its call id, or None when
    nothing was pushed. Never raises: this runs inside the user's forward.

    A `super().__call__(...)` from a wrapped subclass reaches the parent's
    wrapper with the same instance already on top of the stack: that is one
    call, not two, so it is not pushed again.
    """
    try:
        stack = _stack()
        obj_id = id(module)
        if stack and stack[-1].get("obj_id") == obj_id:
            return None
        cid = _push(_qualname_for(module), type(module).__name__, id_of=obj_id)
        stack[-1]["obj_id"] = obj_id
        return cid
    except Exception:
        return None


def _exit_module(cid: int | None) -> None:
    if cid is None:
        return
    try:
        _pop(cid)
    except Exception:
        pass


def _wrap_class(cls: type) -> None:
    """Wrap a single nn.Module subclass's __call__ in place."""
    original = cls.__dict__.get("__call__")
    if original is None:
        return
    if getattr(original, "_smeltr_wrapped", False):
        return

    # functools.wraps keeps the name, docstring and `__wrapped__`, so
    # `inspect.signature(model.__call__)` still reports the user's
    # parameters: mlx_lm probes it to decide which inputs a model takes.
    @functools.wraps(original)
    def wrapped(self, *args, **kwargs):
        cid = _enter_module(self)
        try:
            return original(self, *args, **kwargs)
        finally:
            _exit_module(cid)

    wrapped._smeltr_wrapped = True  # type: ignore[attr-defined]
    wrapped._smeltr_original = original  # type: ignore[attr-defined]
    cls.__call__ = wrapped  # type: ignore[assignment]
    with _wrapped_classes_lock:
        _wrapped_classes.append(cls)


def _wrap_all_existing(base: type) -> None:
    """Recursively wrap __call__ on all existing subclasses of base."""
    for sub in base.__subclasses__():
        _wrap_class(sub)
        _wrap_all_existing(sub)


def install() -> None:
    """Monkey-patch mlx.nn.Module.__call__. Idempotent.

    No-op if SMELTR_MODULES_DISABLE=1 or if mlx.nn cannot be imported.
    """
    global _installed
    if os.environ.get("SMELTR_MODULES_DISABLE") == "1":
        return
    with _install_lock:
        if _installed:
            return
        try:
            import mlx.nn as nn
        except ImportError:
            warning("smeltr.modules", "mlx.nn not importable - module tracking disabled")
            return

        # Wrap all currently known subclasses.
        _wrap_all_existing(nn.Module)

        # Install __init_subclass__ hook so future subclasses are wrapped too.
        _install_subclass_hook(nn.Module)

        # nn.Module itself defines no __call__: a subclass without one must
        # stay non-callable, and one inheriting a wrapped __call__ is already
        # tracked through it. Wrap it only if a future MLX adds one.
        _wrap_class(nn.Module)

        _installed = True


def _install_subclass_hook(base: type) -> None:
    """Add an __init_subclass__ hook that auto-wraps future subclasses."""
    original_isc = base.__dict__.get("__init_subclass__")

    @classmethod  # type: ignore[misc]
    def patched_isc(cls, **kwargs):
        if original_isc is not None:
            original_isc.__func__(cls, **kwargs)
        else:
            super(base, cls).__init_subclass__(**kwargs)
        _wrap_class(cls)

    patched_isc._smeltr_original_isc = original_isc  # type: ignore[attr-defined]
    base.__init_subclass__ = patched_isc  # type: ignore[assignment]


def uninstall() -> None:
    """Restore the original mlx.nn.Module.__call__. Safe if not installed."""
    global _installed, _wrapped_classes
    with _install_lock:
        if not _installed:
            return
        try:
            import mlx.nn as nn
        except ImportError:
            _installed = False
            return

        # Snapshot and clear _wrapped_classes under its own lock to prevent
        # races with concurrent __init_subclass__ calls.
        with _wrapped_classes_lock:
            to_restore = list(_wrapped_classes)
            _wrapped_classes = []

        # Restore all wrapped classes.
        for cls in to_restore:
            current = cls.__dict__.get("__call__")
            if current is not None and getattr(current, "_smeltr_wrapped", False):
                original = getattr(current, "_smeltr_original", None)
                if original is not None:
                    cls.__call__ = original  # type: ignore[assignment]
                else:
                    try:
                        del cls.__call__
                    except AttributeError:
                        pass

        # Remove __init_subclass__ hook.
        isc = nn.Module.__dict__.get("__init_subclass__")
        if isc is not None and hasattr(isc, "_smeltr_original_isc"):
            original_isc = isc._smeltr_original_isc
            if original_isc is not None:
                nn.Module.__init_subclass__ = original_isc  # type: ignore[assignment]
            else:
                try:
                    del nn.Module.__init_subclass__
                except AttributeError:
                    pass

        _installed = False


def _reset_for_tests() -> None:
    """Reset all module-level state. For tests only."""
    global _call_counter
    uninstall()
    _call_counter = itertools.count(1)
    _tls.stack = []


def _after_fork_in_child() -> None:
    # A lock held by another thread at fork() would stay held in the child.
    global _install_lock, _wrapped_classes_lock
    _install_lock = threading.Lock()
    _wrapped_classes_lock = threading.RLock()


if hasattr(os, "register_at_fork"):
    os.register_at_fork(after_in_child=_after_fork_in_child)

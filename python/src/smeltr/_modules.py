"""Module-call tracking: monkey-patch mlx.nn.Module.__call__ to emit
ModuleEntered/ModuleReturned events and maintain a thread-local stack
that the mx.eval hook can snapshot into MlxEvalEntered.module_stack.
"""

from __future__ import annotations

import logging
import os
import threading
from typing import Any

from smeltr._api import _emit as _api_emit

_log = logging.getLogger("smeltr.modules")

_tls = threading.local()
_call_counter = 0
_call_counter_lock = threading.Lock()
_installed = False
_install_lock = threading.Lock()

# Track which classes have been wrapped so we can uninstall cleanly.
_wrapped_classes: list[type] = []
# Dedicated lock for _wrapped_classes mutations (avoids reentrant deadlock with
# _install_lock, which is held for the full duration of install()).
_wrapped_classes_lock = threading.RLock()

# Saved at _install_base_sentinel() time; used as fallback in base_sentinel.
_original_module_call: Any = None


def _emit(payload: dict[str, Any]) -> None:
    """Thin indirection so tests can patch it cleanly."""
    try:
        _api_emit(payload)
    except Exception:
        # Observability must never break the user's code.
        pass


def _next_call_id() -> int:
    global _call_counter
    with _call_counter_lock:
        _call_counter += 1
        return _call_counter


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
        else:
            out[k] = str(v)
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
    label = getattr(module, "name", None) or cls
    return cls if label == cls else f"{cls}:{label}"


def _wrap_class(cls: type) -> None:
    """Wrap a single nn.Module subclass's __call__ in place."""
    original = cls.__dict__.get("__call__")
    if original is None:
        return
    if getattr(original, "_smeltr_wrapped", False):
        return

    def wrapped(self, *args, **kwargs):
        cid = _push(
            _qualname_for(self),
            type(self).__name__,
            id_of=id(self),
        )
        try:
            return original(self, *args, **kwargs)
        finally:
            _pop(cid)

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
            _log.warning("mlx.nn not importable - module tracking disabled")
            return

        # Wrap all currently known subclasses.
        _wrap_all_existing(nn.Module)

        # Install __init_subclass__ hook so future subclasses are wrapped too.
        _install_subclass_hook(nn.Module)

        # Install a sentinel __call__ on the base class so that:
        #   (a) getattr(nn.Module.__call__, "_smeltr_wrapped", False) is True, and
        #   (b) classes with no own __call__ are still intercepted.
        _install_base_sentinel(nn.Module)

        _installed = True


def _install_base_sentinel(base: type) -> None:
    """Install a Python __call__ on the base Module class as a sentinel.

    This serves two purposes:
    1. Makes ``getattr(nn.Module.__call__, "_smeltr_wrapped", False)`` return True
       (required by the idempotency test).
    2. Intercepts calls on subclasses that do NOT define their own __call__.
    """
    global _original_module_call
    # Save the original __call__ from the base class dict (the C method-wrapper)
    # before we overwrite it.  Used as a fallback in base_sentinel when no
    # Python __call__ is found in the MRO.
    _original_module_call = base.__dict__.get("__call__")

    # There is no Python __call__ on nn.Module itself (it's a C method-wrapper
    # from dict/object), so we define one.  Subclasses with their own __call__
    # already have been wrapped by _wrap_all_existing; this covers the rest.
    def base_sentinel(self, *args, **kwargs):  # pragma: no cover
        cid = _push(
            _qualname_for(self),
            type(self).__name__,
            id_of=id(self),
        )
        try:
            # Walk the MRO to find a real __call__.  Skip nn.Module itself
            # (that's us), skip any class that only has base_sentinel or a
            # smeltr-wrapped shim (to avoid infinite recursion).
            for cls in type(self).__mro__:
                if cls is base:
                    continue
                call = cls.__dict__.get("__call__")
                if call is None:
                    continue
                if call is base_sentinel:
                    continue
                # A smeltr-wrapped __call__ has already been handled by
                # _wrap_class; call it directly so it can do its own tracking.
                return call(self, *args, **kwargs)
            # Fallback: call the original C-level __call__ saved at install time.
            if _original_module_call is not None:
                return _original_module_call(self, *args, **kwargs)
            raise AttributeError(f"{type(self).__name__}: no __call__ found in MRO")
        finally:
            _pop(cid)

    base_sentinel._smeltr_wrapped = True  # type: ignore[attr-defined]
    base.__call__ = base_sentinel  # type: ignore[assignment]
    with _wrapped_classes_lock:
        _wrapped_classes.append(base)


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
    global _installed, _wrapped_classes, _original_module_call
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

        _original_module_call = None

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
    with _call_counter_lock:
        _call_counter = 0
    _tls.stack = []

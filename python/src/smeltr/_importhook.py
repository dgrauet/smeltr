"""Run a callback once the program imports a module (#266).

Autoload attaches to every Python process `smeltr record` starts, launchers
included. Importing mlx itself to instrument it would load the Metal
backend (`import mlx.core` allocates a Metal heap) into processes that never
touch MLX, and import torch through `safetensors.torch`. Instead, the
mlx-dependent installs wait here until the program imports those modules.

A `sys.meta_path` finder resolves the watched module through the other
finders and wraps its loader just long enough to run the callbacks after
the module has executed; the module keeps its real loader.
"""

from __future__ import annotations

import sys
import threading
from collections.abc import Callable
from types import ModuleType
from typing import Any

_pending: dict[str, list[Callable[[ModuleType], None]]] = {}
_lock = threading.Lock()


def when_imported(name: str, callback: Callable[[ModuleType], None]) -> None:
    """Call `callback(module)` once `name` is imported — right away if it
    already is. The callback runs inside the program's import statement:
    any exception it raises is logged, never propagated."""
    module = sys.modules.get(name)
    if module is not None:
        _run(callback, module)
        return
    with _lock:
        _pending.setdefault(name, []).append(callback)
        if _FINDER not in sys.meta_path:
            sys.meta_path.insert(0, _FINDER)


def cancel_all() -> None:
    """Forget every pending callback (detach)."""
    with _lock:
        _pending.clear()
        _remove_finder()


def _remove_finder() -> None:
    try:
        sys.meta_path.remove(_FINDER)
    except ValueError:
        pass


def _run(callback: Callable[[ModuleType], None], module: ModuleType) -> None:
    try:
        callback(module)
    except Exception as e:
        from smeltr._log import warning

        warning("smeltr.importhook", "smeltr: instrumenting %s failed: %s", module.__name__, e)


def _fire(name: str, module: ModuleType) -> None:
    with _lock:
        callbacks = _pending.pop(name, [])
        if not _pending:
            _remove_finder()
    for cb in callbacks:
        _run(cb, module)


class _Finder:
    def find_spec(self, fullname: str, path: Any = None, target: Any = None) -> Any:
        if fullname not in _pending:
            return None
        for finder in list(sys.meta_path):
            if finder is self:
                continue
            find = getattr(finder, "find_spec", None)
            if find is None:
                continue
            try:
                spec = find(fullname, path, target)
            except Exception:
                return None  # let the regular machinery report it
            if spec is not None:
                break
        else:
            return None
        loader = spec.loader
        if loader is None or not hasattr(loader, "exec_module"):
            return None
        spec.loader = _NotifyingLoader(loader, fullname)
        return spec


class _NotifyingLoader:
    def __init__(self, loader: Any, name: str) -> None:
        self._loader = loader
        self._name = name

    def create_module(self, spec: Any) -> Any:
        return self._loader.create_module(spec)

    def exec_module(self, module: ModuleType) -> None:
        # From here on the module sees its real loader (importlib.resources
        # and friends read `__spec__.loader`).
        spec = getattr(module, "__spec__", None)
        if spec is not None:
            spec.loader = self._loader
        if getattr(module, "__loader__", None) is self:
            module.__loader__ = self._loader
        self._loader.exec_module(module)
        _fire(self._name, module)

    def __getattr__(self, attr: str) -> Any:
        return getattr(self._loader, attr)


_FINDER = _Finder()

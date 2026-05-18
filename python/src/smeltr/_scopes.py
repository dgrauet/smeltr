"""User-defined profiling scopes.

`smeltr.scope("name")` pushes a frame onto the same thread-local stack used
by mlx.nn.Module call tracking (`smeltr._modules`), so kernels dispatched
on this thread during the scope are attributed to "name" by the analyzer's
breakdown tree.

Reuses the existing ModuleEntered/ModuleReturned event plumbing — no new
event variants, no Rust changes.
"""

from __future__ import annotations

import contextlib
import functools
from collections.abc import Callable, Generator
from typing import Any, TypeVar

from smeltr import _modules

_SCOPE_CLASS_NAME = "Scope"

F = TypeVar("F", bound=Callable[..., Any])


@contextlib.contextmanager
def _scope_cm(name: str) -> Generator[None, None, None]:
    cid = _modules._push(name, _SCOPE_CLASS_NAME, id_of=id(name))
    try:
        yield
    finally:
        _modules._pop(cid)


def _scope_decorator(name: str) -> Callable[[F], F]:
    def decorator(fn: F) -> F:
        @functools.wraps(fn)
        def wrapper(*args: Any, **kwargs: Any) -> Any:
            with _scope_cm(name):
                return fn(*args, **kwargs)

        return wrapper  # type: ignore[return-value]

    return decorator


class _Scope:
    """Dispatcher: `smeltr.scope("x")` is usable both as a context manager
    (`with smeltr.scope("x"):`) and as a decorator (`@smeltr.scope("x")`).
    """

    def __init__(self, name: str) -> None:
        self._name = name
        self._cm: contextlib.AbstractContextManager[None] | None = None

    def __enter__(self) -> None:
        assert self._cm is None, (
            "smeltr.scope() instance is not re-entrant; "
            "call smeltr.scope(name) again to obtain a fresh scope"
        )
        self._cm = _scope_cm(self._name)
        self._cm.__enter__()

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> bool | None:
        assert self._cm is not None
        try:
            return self._cm.__exit__(exc_type, exc, tb)
        finally:
            self._cm = None

    def __call__(self, fn: F) -> F:
        return _scope_decorator(self._name)(fn)


def scope(name: str) -> _Scope:
    """Annotate a block of code as a profiling scope.

    Usage:

        with smeltr.scope("denoise.pass:cond"):
            cond_x0 = model(**cond_kwargs)

        @smeltr.scope("forward")
        def forward(self, x): ...

    Kernels dispatched on this thread while the scope is active are
    attributed to this scope by smeltrd's breakdown analyzer. Scopes nest
    freely and interleave with mlx.nn.Module call tracking.

    No-op when smeltr.attach() has not been called.
    """
    return _Scope(name)

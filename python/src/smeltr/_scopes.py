"""User-defined profiling scopes.

`smeltr.scope("name", **fields)` pushes a frame onto the same thread-local
stack used by mlx.nn.Module call tracking (`smeltr._modules`), so kernels
dispatched on this thread during the scope are attributed to "name" by
the analyzer's breakdown tree.

Optional `**fields` are propagated into the `ModuleEntered` event payload
as structured metadata (bool/int/float/str; other types are stringified).
"""

from __future__ import annotations

import contextlib
import functools
import inspect
from collections.abc import Callable, Generator
from typing import Any, TypeVar, cast

from smeltr import _modules

_SCOPE_CLASS_NAME = "Scope"

F = TypeVar("F", bound=Callable[..., Any])


@contextlib.contextmanager
def _scope_cm(name: str, fields: dict[str, Any] | None = None) -> Generator[None, None, None]:
    cid = _modules._push(name, _SCOPE_CLASS_NAME, id_of=id(name), fields=fields)
    try:
        yield
    finally:
        _modules._pop(cid)


def _scope_decorator(name: str, fields: dict[str, Any] | None = None) -> Callable[[F], F]:
    def decorator(fn: F) -> F:
        if inspect.iscoroutinefunction(fn) or inspect.isasyncgenfunction(fn):
            raise TypeError(
                f"smeltr.scope() decorator does not support async functions "
                f"({fn.__qualname__!r}); use `with smeltr.scope({name!r}, ...): ...` "
                f"inside the async body, or wrap the eval call directly."
            )
        if inspect.isgeneratorfunction(fn):
            raise TypeError(
                f"smeltr.scope() decorator does not support generator functions "
                f"({fn.__qualname__!r}); use `with smeltr.scope({name!r}, ...): ...` "
                f"inside the generator body."
            )

        @functools.wraps(fn)
        def wrapper(*args: Any, **kwargs: Any) -> Any:
            with _scope_cm(name, fields):
                return fn(*args, **kwargs)

        return cast(F, wrapper)

    return decorator


class _Scope:
    """Dispatcher: `smeltr.scope("x", k=v)` is usable both as a context
    manager (`with smeltr.scope("x", k=v):`) and as a decorator
    (`@smeltr.scope("x", k=v)`).
    """

    def __init__(self, name: str, fields: dict[str, Any] | None = None) -> None:
        self._name = name
        self._fields = fields
        self._cm: contextlib.AbstractContextManager[None] | None = None

    def __enter__(self) -> None:
        assert self._cm is None, (
            "smeltr.scope() instance is not re-entrant; "
            "call smeltr.scope(name) again to obtain a fresh scope"
        )
        self._cm = _scope_cm(self._name, self._fields)
        self._cm.__enter__()

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> bool | None:
        assert self._cm is not None
        try:
            return self._cm.__exit__(exc_type, exc, tb)
        finally:
            self._cm = None

    def __call__(self, fn: F) -> F:
        return _scope_decorator(self._name, self._fields)(fn)


def scope(name: str, **fields: Any) -> _Scope:
    """Annotate a block of code as a profiling scope.

    Usage:

        with smeltr.scope("denoise.step", step=step_idx, sigma=float(sigma)):
            cond_x0 = model(**cond_kwargs)

        @smeltr.scope("forward", layer=3)
        def forward(self, x): ...

    Kernels dispatched on this thread while the scope is active are
    attributed to this scope by smeltrd's breakdown analyzer. Scopes nest
    freely and interleave with mlx.nn.Module call tracking.

    Field values must be bool/int/float/str; any other type is stringified
    via str(). No-op when smeltr.attach() has not been called.
    """
    return _Scope(name, fields if fields else None)

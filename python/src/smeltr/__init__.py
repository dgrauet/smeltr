"""smeltr - Python sidecar for the smeltr Metal/MLX observability tool.

Public API (spec section 4.4):
    attach(), detach(), session(name), mark(label, **fields), now(),
    snapshot(), decorate_eval(), panic_on(condition)
"""

from smeltr._version import __version__


def attach(*args, **kwargs):
    raise NotImplementedError("attach() is implemented in a later task")


def detach(*args, **kwargs):
    raise NotImplementedError("detach() is implemented in a later task")


def session(*args, **kwargs):
    raise NotImplementedError("session() is implemented in a later task")


def mark(*args, **kwargs):
    raise NotImplementedError("mark() is implemented in a later task")


def now(*args, **kwargs):
    raise NotImplementedError("now() is implemented in a later task")


def snapshot(*args, **kwargs):
    raise NotImplementedError("snapshot() is implemented in a later task")


def decorate_eval(*args, **kwargs):
    raise NotImplementedError("decorate_eval() is implemented in a later task")


def panic_on(*args, **kwargs):
    raise NotImplementedError("panic_on() is implemented in a later task")


__all__ = [
    "__version__", "attach", "detach", "session", "mark", "now",
    "snapshot", "decorate_eval", "panic_on",
]

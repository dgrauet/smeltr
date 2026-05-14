"""smeltr - Python sidecar for the smeltr Metal/MLX observability tool."""

from smeltr._api import attach, detach, mark, now, session
from smeltr._version import __version__


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

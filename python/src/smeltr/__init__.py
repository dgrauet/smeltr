"""smeltr - Python sidecar for the smeltr Metal/MLX observability tool."""

from smeltr._api import attach, detach, export, mark, now, session
from smeltr._mlx import decorate_eval, snapshot
from smeltr._scopes import scope
from smeltr._shutdown import panic_on
from smeltr._version import __version__

__all__ = [
    "__version__",
    "attach",
    "decorate_eval",
    "detach",
    "export",
    "mark",
    "now",
    "panic_on",
    "scope",
    "session",
    "snapshot",
]

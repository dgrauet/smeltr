"""Auto-attach hook triggered by site.py via smeltr-autoload.pth.

Imported only when SMELTR_AUTOLOAD=1 is in the environment. The `smeltr record`
CLI command sets this variable in the child process it spawns, so user code
under `smeltr record python script.py` is observed without any modification.

In any other Python invocation (pytest, notebooks, unrelated tools that
happen to import smeltr), the variable is unset and this module does
nothing — preserving the rule that observability must never break user code.
"""

from __future__ import annotations

import os

from smeltr._log import warning


def _activate() -> None:
    if os.environ.get("SMELTR_AUTOLOAD") != "1":
        return
    try:
        from smeltr._api import attach

        # mlx and safetensors are instrumented when the program imports
        # them: importing them here would load the Metal backend (and torch)
        # into launchers and helpers that never use them (#266).
        attach(_autoload=True)
    except Exception as exc:
        # Observability must never break user code.
        warning("smeltr.autoload", "smeltr autoload failed: %s", exc)


_activate()

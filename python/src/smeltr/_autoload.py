"""Auto-attach hook triggered by site.py via smeltr-autoload.pth.

Active only when SMELTR_AUTOLOAD=1 is in the environment. The `smeltr record`
CLI command sets this variable in the child process it spawns, so user code
under `smeltr record python script.py` is observed without any modification.

In any other Python invocation (pytest, notebooks, unrelated tools that
happen to import smeltr), the variable is unset and this module does
nothing — preserving the rule that observability must never break user code.
"""

from __future__ import annotations

import logging
import os

_log = logging.getLogger("smeltr.autoload")


def _activate() -> None:
    if os.environ.get("SMELTR_AUTOLOAD") != "1":
        return
    try:
        from smeltr._api import attach
        from smeltr._mlx import decorate_eval

        attach()
        decorate_eval()
    except Exception as exc:
        # Observability must never break user code.
        _log.warning("smeltr autoload failed: %s", exc)


_activate()

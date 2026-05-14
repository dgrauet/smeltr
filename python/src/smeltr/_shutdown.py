"""Shutdown hooks: atexit, SIGTERM, sys.excepthook, panic_on()."""

from __future__ import annotations

import atexit
import os
import queue
import signal
import sys
import threading
from collections.abc import Callable

_atexit_registered = False
_original_excepthook = None
_original_sigterm = None
_panic_thread: threading.Thread | None = None
_panic_stop = threading.Event()
_panic_queue: queue.Queue = queue.Queue()


def _atexit_handler() -> None:
    try:
        from smeltr._mlx import snapshot

        snapshot()
    except Exception:
        pass
    try:
        from smeltr._api import detach

        detach()
    except Exception:
        pass


def _excepthook(exc_type, exc_value, exc_tb) -> None:
    try:
        from smeltr._api import mark

        mark(f"uncaught: {exc_type.__name__}: {exc_value}")
        from smeltr._mlx import snapshot

        snapshot()
    except Exception:
        pass
    if _original_excepthook is not None:
        _original_excepthook(exc_type, exc_value, exc_tb)


def _sigterm_handler(signum, frame) -> None:
    _atexit_handler()
    signal.signal(signal.SIGTERM, signal.SIG_DFL)
    signal.raise_signal(signal.SIGTERM)


def install_hooks() -> None:
    global _atexit_registered, _original_excepthook, _original_sigterm
    if not _atexit_registered:
        atexit.register(_atexit_handler)
        _atexit_registered = True
    if _original_excepthook is None:
        _original_excepthook = sys.excepthook
        sys.excepthook = _excepthook
    if _original_sigterm is None:
        try:
            _original_sigterm = signal.signal(signal.SIGTERM, _sigterm_handler)
        except ValueError:
            _original_sigterm = None


def remove_hooks() -> None:
    global _original_excepthook, _original_sigterm
    if _original_excepthook is not None:
        sys.excepthook = _original_excepthook
        _original_excepthook = None
    if _original_sigterm is not None:
        try:
            signal.signal(signal.SIGTERM, _original_sigterm)
        except ValueError:
            pass
        _original_sigterm = None
    stop_panic()


def panic_on(
    predicate: Callable[[], bool], *, check_every_s: float = 0.5, _exit_via_os: bool = True
) -> None:
    """Watchdog: when predicate() is True, snapshot and exit(99).

    _exit_via_os=False is for tests; the SystemExit is queued instead of
    calling os._exit (which would terminate the test runner).
    """
    global _panic_thread
    stop_panic()
    _panic_stop.clear()

    def _loop():
        from smeltr._api import _emit
        from smeltr._mlx import snapshot as _snap

        while not _panic_stop.is_set():
            try:
                fired = bool(predicate())
            except Exception:
                fired = False
            if fired:
                try:
                    _emit(
                        {
                            "kind": "MlxPanicTriggered",
                            "condition": getattr(predicate, "__name__", repr(predicate)),
                        }
                    )
                    _snap()
                except Exception:
                    pass
                if _exit_via_os:
                    os._exit(99)
                _panic_queue.put(SystemExit(99))
                return
            _panic_stop.wait(check_every_s)

    _panic_thread = threading.Thread(target=_loop, daemon=True, name="smeltr-panic-on")
    _panic_thread.start()


def stop_panic() -> None:
    global _panic_thread
    _panic_stop.set()
    if _panic_thread is not None:
        _panic_thread.join(timeout=1.0)
    _panic_thread = None


def _drain_panic_for_tests() -> None:
    try:
        exc = _panic_queue.get(timeout=1.0)
    except queue.Empty:
        return
    raise exc

"""Shutdown hooks: atexit, SIGTERM, sys.excepthook, panic_on()."""

from __future__ import annotations

import atexit
import os
import queue
import signal
import sys
import threading
from collections.abc import Callable
from typing import Any

_atexit_registered = False
_original_excepthook = None
# The SIGTERM disposition attach() found (SIG_DFL or the program's handler),
# chained by _sigterm_handler and restored by remove_hooks().
_prev_sigterm: Any = None
_sigterm_installed = False
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
    prev = _prev_sigterm
    if callable(prev):
        # The program handles SIGTERM itself — it may shut down gracefully or
        # carry on. Record the moment, then hand over; stay attached (its
        # exit, if any, runs the atexit hook).
        try:
            from smeltr._api import mark
            from smeltr._mlx import snapshot

            mark("signal: SIGTERM")
            snapshot()
        except Exception:
            pass
        prev(signum, frame)
        return
    # Default disposition: flush, then terminate as the program would have.
    _atexit_handler()
    signal.signal(signal.SIGTERM, signal.SIG_DFL)
    signal.raise_signal(signal.SIGTERM)


def install_hooks() -> None:
    global _atexit_registered, _original_excepthook, _sigterm_installed, _prev_sigterm
    if not _atexit_registered:
        atexit.register(_atexit_handler)
        _atexit_registered = True
    if _original_excepthook is None:
        _original_excepthook = sys.excepthook
        sys.excepthook = _excepthook
    if not _sigterm_installed:
        current = signal.getsignal(signal.SIGTERM)
        # SIG_IGN: the program ignores SIGTERM, and must keep doing so (#266).
        # None: a handler installed outside Python, which we cannot chain.
        if current is signal.SIG_IGN or current is None:
            return
        try:
            signal.signal(signal.SIGTERM, _sigterm_handler)
        except ValueError:
            return  # not the main thread
        _prev_sigterm = current
        _sigterm_installed = True


def remove_hooks() -> None:
    global _original_excepthook, _sigterm_installed, _prev_sigterm
    if _original_excepthook is not None:
        if sys.excepthook is _excepthook:
            sys.excepthook = _original_excepthook
        _original_excepthook = None
    if _sigterm_installed:
        # Only undo our own handler: one the program installed since attach()
        # stays.
        if signal.getsignal(signal.SIGTERM) is _sigterm_handler:
            try:
                signal.signal(signal.SIGTERM, _prev_sigterm)
            except (ValueError, TypeError):
                pass
        _sigterm_installed = False
        _prev_sigterm = None
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

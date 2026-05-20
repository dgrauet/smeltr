"""Public API surface: attach, detach, session, mark, now."""

from __future__ import annotations

import contextlib
import platform
import sys
import threading
import time
from collections.abc import Generator

from smeltr._client import ClientError, _Client
from smeltr._proto import SOURCE_PYTHON_SIDECAR

_client: _Client | None = None
_client_lock = threading.Lock()
_scope_token: str | None = None


def _detect_mlx_version() -> str | None:
    """Returns the installed MLX version, or None if MLX is unavailable.

    MLX 0.30+ removed `mlx.__version__`; fall back to importlib.metadata
    which works for any pip-installed package.
    """
    try:
        import mlx  # noqa: F401  # confirm package is importable first
    except ImportError:
        return None
    try:
        import importlib.metadata as _md

        return _md.version("mlx")
    except Exception:  # PackageNotFoundError or other
        pass
    try:
        import mlx as _mlx_module

        return getattr(_mlx_module, "__version__", None)
    except ImportError:
        return None


def attach(client_name: str = "smeltr-py", timeout_s: float = 2.0, poll_hz: float = 1.0) -> None:
    """Connect to smeltrd. poll_hz is wired up in a later task; accept it now
    so callers don't need to change later."""
    global _client, _scope_token
    import os

    _scope_token = os.environ.get("SMELTR_SCOPE_TOKEN")
    with _client_lock:
        if _client is not None:
            _client.close()
        c = _Client(client_name=client_name)
        c.connect(timeout_s=timeout_s)
        _client = c
    try:
        _emit(
            {
                "kind": "PythonSidecarHello",
                "python_version": platform.python_version(),
                "mlx_version": _detect_mlx_version(),
                "argv": list(sys.argv),
            }
        )
    except ClientError:
        pass
    from smeltr._mlx import start_polling

    start_polling(poll_hz)
    from smeltr._shutdown import install_hooks

    install_hooks()
    from smeltr._modules import install as _install_modules

    _install_modules()


def detach() -> None:
    """Close the daemon connection. Idempotent."""
    from smeltr._modules import uninstall as _uninstall_modules

    _uninstall_modules()
    from smeltr._shutdown import remove_hooks

    remove_hooks()
    from smeltr._mlx import stop_polling

    stop_polling()
    global _client, _scope_token
    with _client_lock:
        if _client is not None:
            _client.close()
            _client = None
        _scope_token = None


def _require_attached() -> _Client:
    if _client is None:
        raise RuntimeError("smeltr.attach() must be called first")
    return _client


def _emit(payload: dict, *, pid: int | None = None) -> None:
    c = _require_attached()
    if pid is None:
        import os

        pid = os.getpid()
    c.emit(
        payload,
        pid=pid,
        scope_token=_scope_token,
        source=SOURCE_PYTHON_SIDECAR,
    )


def mark(label: str, **fields: object) -> None:
    """Drop a labelled event on the timeline.

    Optional `**fields` are propagated as structured metadata in the
    `Mark` event payload (same `FieldValue` enum used by `scope`).
    Non-primitive types are stringified via `str()` so the call never
    raises. Old behavior (JSON-encoding into the label string) is
    removed — consumers querying `Mark.label` for legacy JSON suffixes
    will need to read `Mark.fields` instead.
    """
    payload: dict[str, object] = {"kind": "Mark", "label": label}
    if fields:
        from smeltr._modules import _coerce_fields

        payload["fields"] = _coerce_fields(fields)
    _emit(payload)


def now() -> int:
    """Return a monotonic timestamp in ns aligned with the daemon clock."""
    return time.monotonic_ns()


@contextlib.contextmanager
def session(name: str) -> Generator[None, None, None]:
    _require_attached()
    mark(f"session-open: {name}")
    try:
        yield
    finally:
        mark(f"session-close: {name}")


def export(
    filepath: str,
    format: str = "chrome-trace",
    session: str | None = None,
) -> None:
    """Export a recorded session to a structured file via the `smeltr` CLI.

    Args:
        filepath: Output path on disk. The CLI will write to this path.
        format: "chrome-trace" (default, openable in chrome://tracing /
            Perfetto / Speedscope) or "json" (raw event dump).
        session: Session reference (short id, UUID, or name). Defaults to
            the active session known by the connected daemon (set up at
            attach()).

    Raises:
        RuntimeError: if smeltr is not attached and `session` is None, the
            daemon does not expose an active session, the `smeltr` CLI is
            not on PATH, or the subprocess exits non-zero.
    """
    import shutil
    import subprocess

    resolved_session = session
    if resolved_session is None:
        if _client is None:
            raise RuntimeError("smeltr.attach() must be called first, or pass session=… explicitly")
        active = _client.active_session
        if not active:
            raise RuntimeError("no active session known by the daemon; pass session=… explicitly")
        resolved_session = active

    smeltr_bin = shutil.which("smeltr")
    if smeltr_bin is None:
        raise RuntimeError(
            "smeltr CLI not found on PATH; install smeltr or invoke the CLI directly"
        )

    cmd = [
        smeltr_bin,
        "export",
        resolved_session,
        "--format",
        format,
        "--output",
        filepath,
    ]
    try:
        subprocess.run(cmd, check=True, capture_output=True)
    except subprocess.CalledProcessError as e:
        stderr = e.stderr.decode("utf-8", errors="replace") if e.stderr else ""
        stdout = e.stdout.decode("utf-8", errors="replace") if e.stdout else ""
        raise RuntimeError(
            f"smeltr export failed (exit {e.returncode}): {stderr.strip() or stdout.strip()}"
        ) from e

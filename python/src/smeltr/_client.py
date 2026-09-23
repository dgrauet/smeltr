"""Low-level Unix-socket client with CBOR length-prefixed framing.

Wire format mirrors smeltr_core::codec: each message is `u32_le(len) || cbor`.
"""

from __future__ import annotations

import os
import socket
import struct
import threading
import uuid
from typing import Any

import cbor2

from smeltr._proto import (
    MAX_FRAME_BYTES,
    SOURCE_PYTHON_SIDECAR,
    emit_msg,
    hello_msg,
)

# Longest legitimate hold: one frame write plus one Ack read, each bounded
# by the socket timeout set in connect() (2 s by default).
_LOCK_TIMEOUT_S = 10.0


class ClientError(RuntimeError):
    """Raised when the socket fails or the daemon returns an error."""


def default_socket_path() -> str:
    env = os.environ.get("SMELTR_SOCKET")
    if env:
        return env
    runtime = os.environ.get("XDG_RUNTIME_DIR") or os.environ.get("TMPDIR") or "/tmp"
    return os.path.join(runtime, "smeltr.sock")


class _Client:
    def __init__(self, sock_path: str | None = None, client_name: str = "smeltr-py"):
        self._path = sock_path or default_socket_path()
        self._client_name = client_name
        self._sock: socket.socket | None = None
        self._lock = threading.Lock()
        # Thread currently inside a write/read exchange. A signal handler or
        # GC finalizer that emits runs on an arbitrary thread's stack — often
        # the one mid-exchange — and must not wait for a lock that thread
        # holds (#239), nor interleave a frame into its exchange.
        self._owner: int | None = None
        self.active_session: str | None = None

    def connect(self, timeout_s: float = 2.0, scope_token: str | None = None) -> None:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.settimeout(timeout_s)
        try:
            s.connect(self._path)
        except (FileNotFoundError, ConnectionRefusedError) as e:
            s.close()
            raise ClientError(
                f"could not connect to smeltrd at {self._path}: {e}. "
                f"Is the daemon running? Try `smeltr daemon start`."
            ) from e
        self._sock = s
        self.hello(scope_token)

    def hello(self, scope_token: str | None = None) -> None:
        """(Re-)introduce this client; records the session the daemon says
        its events land in — its recording, when `scope_token` names one
        (#245). May be repeated: the recording can register after attach."""
        if self._sock is None:
            raise ClientError("client is not connected")
        with self._lock:
            self._write_frame(hello_msg(self._client_name, scope_token))
            resp = self._read_frame()
        if not isinstance(resp, dict) or resp.get("kind") != "Welcome":
            raise ClientError(f"unexpected handshake response: {resp!r}")
        ref = resp.get("active_session_ref")
        if not ref:
            # Daemons before 0.28.10 only send `active_session`, the ambient
            # session's UUID as 16 raw bytes.
            raw = resp.get("active_session")
            ref = uuid.UUID(bytes=raw).hex if isinstance(raw, bytes) and len(raw) == 16 else raw
        self.active_session = ref if isinstance(ref, str) and ref else None

    def emit(
        self,
        payload: dict[str, Any],
        *,
        pid: int | None = None,
        scope_token: str | None = None,
        source: str = SOURCE_PYTHON_SIDECAR,
    ) -> None:
        if self._sock is None:
            raise ClientError("client is not connected")
        if self._owner == threading.get_ident():
            raise ClientError("re-entrant emit dropped (signal handler or finalizer)")
        # Bounded: a signal landing between acquire() and the owner store
        # below would otherwise still wait on its own thread forever.
        if not self._lock.acquire(timeout=_LOCK_TIMEOUT_S):
            raise ClientError("emit dropped: client busy")
        try:
            self._owner = threading.get_ident()
            self._write_frame(emit_msg(source, pid, payload, scope_token=scope_token))
            resp = self._read_frame()
        finally:
            self._owner = None
            self._lock.release()
        if not isinstance(resp, dict) or resp.get("kind") != "Ack":
            if isinstance(resp, dict) and resp.get("kind") == "Error":
                raise ClientError(f"daemon error: {resp.get('message')}")
            raise ClientError(f"unexpected emit response: {resp!r}")

    def close(self) -> None:
        if self._sock is not None:
            try:
                self._sock.close()
            finally:
                self._sock = None

    def _write_frame(self, value: dict[str, Any]) -> None:
        assert self._sock is not None
        buf = cbor2.dumps(value)
        if len(buf) > MAX_FRAME_BYTES:
            raise ClientError(f"frame too large: {len(buf)} bytes")
        self._sock.sendall(struct.pack("<I", len(buf)) + buf)

    def _read_frame(self) -> Any:
        assert self._sock is not None
        header = _recv_exact(self._sock, 4)
        (length,) = struct.unpack("<I", header)
        if length > MAX_FRAME_BYTES:
            raise ClientError(f"server frame too large: {length} bytes")
        body = _recv_exact(self._sock, length)
        return cbor2.loads(body)


def _recv_exact(sock: socket.socket, n: int) -> bytes:
    chunks: list[bytes] = []
    remaining = n
    while remaining > 0:
        chunk = sock.recv(remaining)
        if not chunk:
            raise ClientError("connection closed mid-frame")
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)

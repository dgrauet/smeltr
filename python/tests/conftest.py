"""Pytest fixtures: in-process fake smeltrd that speaks the wire protocol."""

from __future__ import annotations

import os
import shutil
import socket
import struct
import tempfile
import threading
import time
from typing import Any

import cbor2
import pytest


class FakeDaemon:
    def __init__(self, sock_path: str):
        self.sock_path = sock_path
        self.received: list[dict[str, Any]] = []
        self.hello_seen = False
        self.hello_tokens: list[str | None] = []
        # What the daemon names as the client's session in its Welcome.
        self.active_session_ref = "ambient1"
        self._listener: socket.socket | None = None
        self._thread: threading.Thread | None = None
        self._stop = threading.Event()
        self._lock = threading.Lock()
        self._conns: list[socket.socket] = []
        # Seconds to wait before acking an Emit: a slow daemon keeps the
        # client inside emit(), holding its lock, most of the time.
        self.ack_delay_s = 0.0
        # Cleared = a stalled daemon (SIGSTOP-like): Emits are read but
        # neither recorded nor acked until it is set again.
        self.gate = threading.Event()
        self.gate.set()

    def start(self) -> None:
        if os.path.exists(self.sock_path):
            os.unlink(self.sock_path)
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        s.bind(self.sock_path)
        s.listen(4)
        s.settimeout(0.2)
        self._listener = s
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._listener is not None:
            self._listener.close()
        if self._thread is not None:
            self._thread.join(timeout=2.0)
        # A stopped daemon drops its clients, like a killed smeltrd.
        with self._lock:
            conns, self._conns = self._conns, []
        for c in conns:
            try:
                c.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        if os.path.exists(self.sock_path):
            os.unlink(self.sock_path)

    def emits(self) -> list[dict[str, Any]]:
        with self._lock:
            return list(self.received)

    def wait_for(self, predicate, timeout_s: float = 3.0) -> bool:
        """Poll until predicate(emits()) holds: the sidecar sends from a
        background thread, so delivery trails the emit call."""
        deadline = time.monotonic() + timeout_s
        while time.monotonic() < deadline:
            if predicate(self.emits()):
                return True
            time.sleep(0.01)
        return predicate(self.emits())

    def _serve(self) -> None:
        while not self._stop.is_set():
            try:
                assert self._listener is not None
                conn, _ = self._listener.accept()
            except (TimeoutError, OSError):
                continue
            threading.Thread(target=self._handle, args=(conn,), daemon=True).start()

    def _handle(self, conn: socket.socket) -> None:
        # Short timeout so the handler notices stop(); an idle client is not
        # an error (the sidecar's connection stays open between events).
        conn.settimeout(0.1)
        with self._lock:
            self._conns.append(conn)
        try:
            while not self._stop.is_set():
                msg = self._read_frame(conn)
                if msg is None:
                    return
                op = msg.get("op")
                if op == "Hello":
                    with self._lock:
                        self.hello_seen = True
                        self.hello_tokens.append(msg.get("scope_token"))
                    self._write_frame(
                        conn,
                        {
                            "kind": "Welcome",
                            "daemon_version": "fake-0.0.1",
                            # The real daemon sends a UUID: 16 raw bytes.
                            "active_session": bytes(16),
                            "active_session_ref": self.active_session_ref,
                        },
                    )
                elif op == "Emit":
                    while not self.gate.wait(0.05):
                        if self._stop.is_set():
                            return
                    with self._lock:
                        self.received.append(msg)
                    if self.ack_delay_s:
                        time.sleep(self.ack_delay_s)
                    self._write_frame(conn, {"kind": "Ack"})
                else:
                    self._write_frame(conn, {"kind": "Error", "message": f"unknown op {op}"})
        except (ConnectionError, OSError):
            return
        finally:
            conn.close()

    def _recv(self, conn: socket.socket, n: int) -> bytes | None:
        buf = b""
        while len(buf) < n:
            try:
                chunk = conn.recv(n - len(buf))
            except TimeoutError:
                if self._stop.is_set():
                    return None
                continue
            if not chunk:
                return None
            buf += chunk
        return buf

    def _read_frame(self, conn: socket.socket) -> dict[str, Any] | None:
        header = self._recv(conn, 4)
        if header is None:
            return None
        (length,) = struct.unpack("<I", header)
        body = self._recv(conn, length)
        if body is None:
            return None
        return cbor2.loads(body)

    @staticmethod
    def _write_frame(conn: socket.socket, value: dict[str, Any]) -> None:
        buf = cbor2.dumps(value)
        conn.sendall(struct.pack("<I", len(buf)) + buf)


@pytest.fixture
def short_tmp_dir():
    # macOS AF_UNIX paths are limited to ~104 chars; pytest's tmp_path is too long.
    d = tempfile.mkdtemp(prefix="smtr-")
    try:
        yield d
    finally:
        shutil.rmtree(d, ignore_errors=True)


@pytest.fixture
def fake_daemon(short_tmp_dir, monkeypatch):
    sock_path = os.path.join(short_tmp_dir, "s.sock")
    monkeypatch.setenv("SMELTR_SOCKET", sock_path)
    d = FakeDaemon(sock_path)
    d.start()
    try:
        yield d
    finally:
        d.stop()


@pytest.fixture(autouse=True)
def _reset_mlx_state():
    # Imported lazily so this fixture survives even before _mlx exists.
    try:
        from smeltr import _mlx

        _mlx._reset_for_tests()
    except (ImportError, AttributeError):
        pass
    yield
    try:
        from smeltr import _mlx

        _mlx._reset_for_tests()
    except (ImportError, AttributeError):
        pass

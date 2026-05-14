"""Pytest fixtures: in-process fake smeltrd that speaks the wire protocol."""

from __future__ import annotations

import os
import shutil
import socket
import struct
import tempfile
import threading
from typing import Any

import cbor2
import pytest


class FakeDaemon:
    def __init__(self, sock_path: str):
        self.sock_path = sock_path
        self.received: list[dict[str, Any]] = []
        self.hello_seen = False
        self._listener: socket.socket | None = None
        self._thread: threading.Thread | None = None
        self._stop = threading.Event()
        self._lock = threading.Lock()

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

    def _serve(self) -> None:
        while not self._stop.is_set():
            try:
                assert self._listener is not None
                conn, _ = self._listener.accept()
            except (socket.timeout, OSError):
                continue
            threading.Thread(target=self._handle, args=(conn,), daemon=True).start()

    def _handle(self, conn: socket.socket) -> None:
        conn.settimeout(1.0)
        try:
            while not self._stop.is_set():
                msg = self._read_frame(conn)
                if msg is None:
                    return
                op = msg.get("op")
                if op == "Hello":
                    with self._lock:
                        self.hello_seen = True
                    self._write_frame(conn, {
                        "kind": "Welcome",
                        "daemon_version": "fake-0.0.1",
                        "active_session": "00000000000000000000000000000001",
                    })
                elif op == "Emit":
                    with self._lock:
                        self.received.append(msg)
                    self._write_frame(conn, {"kind": "Ack"})
                else:
                    self._write_frame(conn, {"kind": "Error",
                                             "message": f"unknown op {op}"})
        except (ConnectionError, OSError):
            return
        finally:
            conn.close()

    @staticmethod
    def _read_frame(conn: socket.socket) -> dict[str, Any] | None:
        header = b""
        while len(header) < 4:
            chunk = conn.recv(4 - len(header))
            if not chunk:
                return None
            header += chunk
        (length,) = struct.unpack("<I", header)
        body = b""
        while len(body) < length:
            chunk = conn.recv(length - len(body))
            if not chunk:
                return None
            body += chunk
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

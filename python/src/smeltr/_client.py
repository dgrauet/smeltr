"""Unix-socket client for smeltrd, with CBOR length-prefixed framing.

Wire format mirrors smeltr_core::codec: each message is `u32_le(len) || cbor`.

An emit never blocks the calling thread on the daemon (#266). `emit()` stamps
the event with `CLOCK_UPTIME_RAW` (the daemon dates it from that stamp) and
appends it to a bounded in-memory queue; a background sender thread writes
the frames in batches and reads their Acks, so the wire protocol is
unchanged. When the queue is full — the daemon is stalled, gone, or slower
than the program — the event is dropped and counted, and the count is
reported to the daemon as a Mark labelled `DROP_REPORT_LABEL` once it
answers again. A failed exchange drops the connection (a half-written frame
or an unread Ack would desynchronise the stream for good) and the sender
reconnects with backoff, e.g. after a daemon restart.

Only Hello/Welcome (`connect()`, `hello()`) are synchronous request/response.

The hot path takes no lock: an emit can come from a signal handler or a GC
finalizer interrupting a thread that is itself emitting (#239), and a
`fork()` while another thread holds a lock would leave it held in the child
forever. `os.register_at_fork` resets the client in the child, which never
writes on the parent's socket and reconnects on its first emit.
"""

from __future__ import annotations

import os
import select
import socket
import struct
import sys
import threading
import time
import uuid
import weakref
from collections import deque
from typing import Any

import cbor2

from smeltr._proto import (
    MAX_FRAME_BYTES,
    SOURCE_PYTHON_SIDECAR,
    emit_msg,
    hello_msg,
)

# Events held while the sender catches up. Beyond this, emits are dropped
# (and counted) rather than blocking the program or growing without bound.
QUEUE_MAX = 8192
# Frames written before reading their Acks. Acks are ~10 bytes, so a batch's
# replies always fit in the socket's receive buffer: no write/write deadlock.
_BATCH_MAX = 64
# Per send/recv, on the sender thread and for Hello.
_IO_TIMEOUT_S = 2.0
_BACKOFF_MIN_S = 0.05
_BACKOFF_MAX_S = 5.0
# Idle sender re-checks its state at least this often.
_IDLE_WAKE_S = 0.5
# Minimum spacing of drop reports while drops keep happening.
_DROP_REPORT_PERIOD_S = 1.0
# How long close() waits for queued events to reach the daemon.
FLUSH_TIMEOUT_S = 1.0

DROP_REPORT_LABEL = "smeltr: sidecar dropped events"

# Darwin: <sys/socket.h> SO_NOSIGPIPE, absent from Python's socket module. A
# write on a socket the daemon closed then fails with EPIPE instead of
# raising SIGPIPE, which kills a program that restored SIG_DFL (exit 141).
_SO_NOSIGPIPE: int | None = getattr(
    socket, "SO_NOSIGPIPE", 0x1022 if sys.platform == "darwin" else None
)

_CLOCK_UPTIME_RAW: int | None = getattr(time, "CLOCK_UPTIME_RAW", None)


def uptime_raw_ns() -> int | None:
    """Now on the daemon's raw clock (`smeltr_core::clock::uptime_raw_ns`,
    i.e. `mach_absolute_time` in ns), or None where it does not exist."""
    if _CLOCK_UPTIME_RAW is None:
        return None
    return time.clock_gettime_ns(_CLOCK_UPTIME_RAW)


class ClientError(RuntimeError):
    """Raised when the socket fails or the daemon returns an error."""


def default_socket_path() -> str:
    env = os.environ.get("SMELTR_SOCKET")
    if env:
        return env
    runtime = os.environ.get("XDG_RUNTIME_DIR") or os.environ.get("TMPDIR") or "/tmp"
    return os.path.join(runtime, "smeltr.sock")


def _wake_pipe() -> tuple[int, int]:
    r, w = os.pipe()
    os.set_blocking(r, False)
    os.set_blocking(w, False)
    return r, w


class _Client:
    def __init__(
        self,
        sock_path: str | None = None,
        client_name: str = "smeltr-py",
        *,
        queue_max: int = QUEUE_MAX,
        io_timeout_s: float = _IO_TIMEOUT_S,
    ):
        self._path = sock_path or default_socket_path()
        self._client_name = client_name
        self._queue_max = queue_max
        self._io_timeout_s = io_timeout_s
        self._scope_token: str | None = None
        self.active_session: str | None = None
        # connect() succeeded and close() has not run: emits are accepted.
        self._opened = False
        self._init_runtime_state()
        _clients.add(self)

    def _init_runtime_state(self) -> None:
        """Everything a forked child must not share with its parent."""
        self._sock: socket.socket | None = None
        # One exchange on the socket at a time: the sender's batches and a
        # user thread's Hello (export) never interleave frames.
        self._io_lock = threading.Lock()
        self._start_lock = threading.Lock()
        self._queue: deque[dict[str, Any]] = deque()
        self._busy = False  # the sender holds a batch taken off the queue
        self._stopping = False
        self._sender: threading.Thread | None = None
        self._sender_idle = False
        self._wake_r, self._wake_w = _wake_pipe()
        # Closed when the client is collected, never earlier: an emit racing
        # close() could otherwise write its wake byte into a reused fd.
        self._pipe_finalizer = weakref.finalize(self, _close_fds, self._wake_r, self._wake_w)
        # Best-effort counts: each is written by one side only.
        self._dropped_emit = 0  # queue full, on the emitting threads
        self._dropped_send = 0  # failed exchanges, on the sender thread
        self._dropped_reported = 0
        self._last_report = 0.0

    @property
    def dropped(self) -> int:
        """Events that did not reach the daemon (or may not have: a failed
        exchange counts its whole batch)."""
        return self._dropped_emit + self._dropped_send

    # ---- synchronous request/response ----

    def connect(self, timeout_s: float | None = None, scope_token: str | None = None) -> None:
        """Connect and say Hello, synchronously; raises ClientError when the
        daemon cannot be reached. Emits are accepted from then on."""
        if timeout_s is not None:
            self._io_timeout_s = timeout_s
        self._scope_token = scope_token
        if not self._io_lock.acquire(timeout=self._io_timeout_s * 2 + 1):
            raise ClientError("client busy")
        try:
            self._open_locked()
        finally:
            self._io_lock.release()
        self._opened = True
        self._ensure_sender()

    def hello(self, scope_token: str | None = None) -> None:
        """(Re-)introduce this client; records the session the daemon says
        its events land in — its recording, when `scope_token` names one
        (#245). May be repeated: the recording can register after attach."""
        if not self._opened:
            raise ClientError("client is not connected")
        self._scope_token = scope_token
        if not self._io_lock.acquire(timeout=self._io_timeout_s * 2 + 1):
            raise ClientError("client busy: the daemon is not answering")
        try:
            if self._sock is None:
                self._open_locked()
                return
            try:
                self._hello_locked()
            except Exception as e:
                self._drop_socket()
                raise ClientError(f"hello failed: {e}") from e
        finally:
            self._io_lock.release()

    def _open_locked(self) -> None:
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            if _SO_NOSIGPIPE is not None:
                try:
                    s.setsockopt(socket.SOL_SOCKET, _SO_NOSIGPIPE, 1)
                except OSError:
                    pass
            s.settimeout(self._io_timeout_s)
            s.connect(self._path)
        except OSError as e:
            s.close()
            raise ClientError(
                f"could not connect to smeltrd at {self._path}: {e}. "
                f"Is the daemon running? Try `smeltr daemon start`."
            ) from e
        self._sock = s
        try:
            self._hello_locked()
        except Exception as e:
            self._drop_socket()
            raise ClientError(f"handshake with smeltrd failed: {e}") from e

    def _hello_locked(self) -> None:
        self._write_frames_locked([_encode(hello_msg(self._client_name, self._scope_token))])
        resp = self._read_frame_locked()
        if not isinstance(resp, dict) or resp.get("kind") != "Welcome":
            raise ClientError(f"unexpected handshake response: {resp!r}")
        ref = resp.get("active_session_ref")
        if not ref:
            # Daemons before 0.28.10 only send `active_session`, the ambient
            # session's UUID as 16 raw bytes.
            raw = resp.get("active_session")
            ref = uuid.UUID(bytes=raw).hex if isinstance(raw, bytes) and len(raw) == 16 else raw
        self.active_session = ref if isinstance(ref, str) and ref else None

    # ---- the hot path ----

    def emit(
        self,
        payload: dict[str, Any],
        *,
        pid: int | None = None,
        scope_token: str | None = None,
        source: str = SOURCE_PYTHON_SIDECAR,
    ) -> None:
        """Queue an event for the daemon. Never blocks and takes no lock."""
        if not self._opened:
            raise ClientError("client is not connected")
        msg = emit_msg(
            source,
            pid,
            payload,
            scope_token=scope_token,
            at_uptime_raw_ns=uptime_raw_ns(),
        )
        queue = self._queue
        if len(queue) >= self._queue_max:
            self._dropped_emit += 1
            return
        queue.append(msg)
        if self._sender is None:
            self._ensure_sender()
        elif self._sender_idle:
            self._wake()

    def _ensure_sender(self) -> None:
        # Non-blocking: a signal handler emitting on a thread that is
        # starting the sender must not wait for itself.
        if not self._start_lock.acquire(blocking=False):
            return
        try:
            if self._sender is None and not self._stopping:
                t = threading.Thread(target=self._run, name="smeltr-sender", daemon=True)
                self._sender = t
                t.start()
        finally:
            self._start_lock.release()

    def _wake(self) -> None:
        try:
            os.write(self._wake_w, b"\0")
        except OSError:
            pass  # pipe full (already woken) or closed

    # ---- the sender thread ----

    def _run(self) -> None:
        backoff = _BACKOFF_MIN_S
        while True:
            try:
                if self._sock is None:
                    if self._stopping:
                        return
                    try:
                        with self._io_lock:
                            self._open_locked()
                    except ClientError:
                        self._sleep(backoff)
                        backoff = min(backoff * 2, _BACKOFF_MAX_S)
                        continue
                    backoff = _BACKOFF_MIN_S
                if self._should_report_drops():
                    self._send_drop_report()
                    continue
                batch = self._take_batch()
                if batch:
                    self._send_batch(batch)
                elif self._stopping:
                    return
                else:
                    self._idle_wait()
            except Exception:
                # Never let the sender die on an unexpected error: drop the
                # connection and start over.
                self._busy = False
                self._drop_socket()
                if self._stopping:
                    return
                self._sleep(backoff)

    def _take_batch(self) -> list[dict[str, Any]]:
        self._busy = True
        batch: list[dict[str, Any]] = []
        queue = self._queue
        try:
            while queue and len(batch) < _BATCH_MAX:
                batch.append(queue.popleft())
        except IndexError:
            pass
        if not batch:
            self._busy = False
        return batch

    def _send_batch(self, batch: list[dict[str, Any]]) -> None:
        try:
            frames: list[bytes] = []
            for msg in batch:
                try:
                    frames.append(_encode(msg))
                except Exception:
                    self._dropped_send += 1
            if frames:
                self._exchange(frames)
        finally:
            self._busy = False

    def _exchange(self, frames: list[bytes]) -> None:
        """Write the frames, then read one reply per frame. A daemon Error
        reply refuses that one event; any I/O failure drops the connection
        and counts the batch as dropped."""
        with self._io_lock:
            if self._sock is None:
                self._dropped_send += len(frames)
                return
            try:
                self._write_frames_locked(frames)
                for _ in frames:
                    self._read_frame_locked()
            except Exception:
                self._drop_socket()
                self._dropped_send += len(frames)

    def _should_report_drops(self) -> bool:
        if self.dropped <= self._dropped_reported:
            return False
        return not self._queue or time.monotonic() - self._last_report >= _DROP_REPORT_PERIOD_S

    def _send_drop_report(self) -> None:
        total = self.dropped
        msg = emit_msg(
            SOURCE_PYTHON_SIDECAR,
            os.getpid(),
            {"kind": "Mark", "label": DROP_REPORT_LABEL, "fields": {"dropped": total}},
            scope_token=self._scope_token,
            at_uptime_raw_ns=uptime_raw_ns(),
        )
        self._last_report = time.monotonic()
        before = self._dropped_send
        self._exchange([_encode(msg)])
        if self._dropped_send == before:
            self._dropped_reported = total
        else:
            # The report itself failed: it is not an event of the program.
            self._dropped_send = before

    def _idle_wait(self) -> None:
        self._sender_idle = True
        try:
            # emit() appends before reading `_sender_idle`, and this reads
            # the queue after setting it: one of the two sees the other.
            if self._queue or self._stopping:
                return
            select.select([self._wake_r], [], [], _IDLE_WAKE_S)
        finally:
            self._sender_idle = False
            self._drain_wake()

    def _sleep(self, seconds: float) -> None:
        """Backoff wait; cut short only by close()."""
        if self._stopping:
            return
        try:
            select.select([self._wake_r], [], [], seconds)
        except (OSError, ValueError):
            time.sleep(seconds)
        self._drain_wake()

    def _drain_wake(self) -> None:
        try:
            while os.read(self._wake_r, 4096):
                pass
        except OSError:
            pass

    # ---- shutdown ----

    def close(self, flush_timeout_s: float = FLUSH_TIMEOUT_S) -> None:
        """Deliver what is queued, waiting at most `flush_timeout_s`, then
        disconnect. Uses no lock the sender may hold, so it is safe from a
        signal handler."""
        sender = self._sender
        if (
            self._opened
            and sender is not None
            and sender.is_alive()
            and sender is not threading.current_thread()
        ):
            # A stopping sender drains the queue, reports its drops, and
            # exits; it no longer reconnects to a daemon that is gone.
            self._stopping = True
            self._wake()
            sender.join(timeout=max(flush_timeout_s, 0.05))
            if sender.is_alive():
                # Stuck in I/O on a stalled daemon: unblock it.
                self._shutdown_socket()
                sender.join(timeout=0.5)
        self._stopping = True
        self._opened = False
        self._dropped_emit += len(self._queue)
        self._queue.clear()
        self._drop_socket()

    def _shutdown_socket(self) -> None:
        sock = self._sock
        if sock is not None:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass

    def _drop_socket(self) -> None:
        """Close the connection without waiting for the I/O lock."""
        sock, self._sock = self._sock, None
        if sock is not None:
            try:
                sock.close()
            except OSError:
                pass

    def _after_fork_in_child(self) -> None:
        """Forget the parent's connection, sender, locks and queue: they
        belong to the parent (its sender still delivers that queue). The
        child reconnects on its first emit."""
        sock = self._sock
        # The child has no other thread that could still use the pipe.
        self._pipe_finalizer()
        self._init_runtime_state()
        if sock is not None:
            # Closes the child's copy of the descriptor only; the parent's
            # connection is untouched (no shutdown()).
            try:
                sock.close()
            except OSError:
                pass

    # ---- framing ----

    def _write_frames_locked(self, frames: list[bytes]) -> None:
        if self._sock is None:
            raise ClientError("not connected")
        self._sock.sendall(b"".join(frames))

    def _read_frame_locked(self) -> Any:
        if self._sock is None:
            raise ClientError("not connected")
        header = _recv_exact(self._sock, 4)
        (length,) = struct.unpack("<I", header)
        if length > MAX_FRAME_BYTES:
            raise ClientError(f"server frame too large: {length} bytes")
        body = _recv_exact(self._sock, length)
        return cbor2.loads(body)


def _close_fds(*fds: int) -> None:
    for fd in fds:
        try:
            os.close(fd)
        except OSError:
            pass


def _encode(value: dict[str, Any]) -> bytes:
    buf = cbor2.dumps(value)
    if len(buf) > MAX_FRAME_BYTES:
        raise ClientError(f"frame too large: {len(buf)} bytes")
    return struct.pack("<I", len(buf)) + buf


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


_clients: weakref.WeakSet[_Client] = weakref.WeakSet()


def _after_fork_in_child() -> None:
    for c in list(_clients):
        try:
            c._after_fork_in_child()
        except Exception:
            pass


if hasattr(os, "register_at_fork"):
    os.register_at_fork(after_in_child=_after_fork_in_child)

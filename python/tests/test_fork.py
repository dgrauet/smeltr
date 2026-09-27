"""#266: a child forked while another thread holds a sidecar lock inherited
it held — 10 s per emit, 20 s per module call — and shared the parent's
socket."""

from __future__ import annotations

import os
import signal
import threading
import time
import warnings

import pytest

import smeltr
from smeltr import _api, _mlx, _modules


def _hold(locks, release: threading.Event, held: threading.Event):
    def run():
        for lock in locks:
            lock.acquire()
        held.set()
        release.wait(10)
        for lock in reversed(locks):
            lock.release()

    t = threading.Thread(target=run, daemon=True)
    t.start()
    assert held.wait(5)
    return t


def _fork_and_run(child_body) -> tuple[int, bytes]:
    """Fork; the child runs child_body() under a 5 s alarm and reports what
    it returns (bytes) through a pipe. Returns (child's exit status, report)."""
    r, w = os.pipe()
    with warnings.catch_warnings():
        warnings.simplefilter("ignore", DeprecationWarning)  # fork with threads
        pid = os.fork()
    if pid == 0:  # pragma: no cover - child
        try:
            os.close(r)
            signal.signal(signal.SIGALRM, signal.SIG_DFL)
            signal.alarm(5)  # a deadlocked child dies instead of hanging
            out = child_body()
            os.write(w, out)
            os._exit(0)
        except BaseException as e:
            os.write(w, repr(e).encode())
            os._exit(1)
    os.close(w)
    deadline = time.monotonic() + 10
    status = None
    while time.monotonic() < deadline:
        done, st = os.waitpid(pid, os.WNOHANG)
        if done:
            status = st
            break
        time.sleep(0.02)
    if status is None:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
        pytest.fail("forked child hung")
    chunks = []
    while True:
        b = os.read(r, 65536)
        if not b:
            break
        chunks.append(b)
    os.close(r)
    return status, b"".join(chunks)


def test_child_forked_while_sidecar_locks_are_held_does_not_stall(fake_daemon):
    smeltr.attach(poll_hz=0)
    try:
        c = _api._client
        assert c is not None
        release, held = threading.Event(), threading.Event()
        locks = [c._io_lock, _mlx._tracked_lock]
        holder = _hold(locks, release, held)
        try:

            def child():
                t = time.perf_counter()
                for i in range(3):
                    smeltr.mark(f"child{i}")
                cid = _modules._push("ChildScope", "Scope", id_of=1)
                _modules._pop(cid)
                _mlx.snapshot()
                elapsed = time.perf_counter() - t
                # os._exit skips atexit: flush explicitly.
                smeltr.detach()
                return f"{elapsed:.3f}".encode()

            status, out = _fork_and_run(child)
        finally:
            release.set()
            holder.join(5)
        assert os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0, out
        assert float(out) < 1.0, f"child emits took {out.decode()} s"
        child_marks = lambda ms: [  # noqa: E731
            m for m in ms if (m["payload"].get("label") or "").startswith("child")
        ]
        # The child reached the daemon on a connection of its own.
        assert fake_daemon.wait_for(lambda ms: len(child_marks(ms)) == 3)
        assert len(fake_daemon.hello_tokens) >= 2
        assert all(m["pid"] != os.getpid() for m in child_marks(fake_daemon.emits()))
        # The parent still works.
        smeltr.mark("parent-after-fork")
        assert fake_daemon.wait_for(
            lambda ms: any(m["payload"].get("label") == "parent-after-fork" for m in ms)
        )
    finally:
        smeltr.detach()


def test_module_call_counter_takes_no_lock_a_fork_could_inherit():
    """`_next_call_id` ran under a plain Lock: a child forked while another
    thread held it would block forever on its first module call."""
    release, held = threading.Event(), threading.Event()
    lock = getattr(_modules, "_call_counter_lock", None) or threading.Lock()
    eval_lock = getattr(_mlx, "_eval_call_counter_lock", None) or threading.Lock()
    holder = _hold([lock, eval_lock], release, held)
    try:

        def child():
            a = _modules._next_call_id()
            b = _mlx._next_call_id()
            return f"{a} {b}".encode()

        status, out = _fork_and_run(child)
    finally:
        release.set()
        holder.join(5)
    assert os.WIFEXITED(status) and os.WEXITSTATUS(status) == 0, (status, out)

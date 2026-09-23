import sys
import time

import pytest

import smeltr


def test_excepthook_emits_uncaught_mark(fake_daemon):
    smeltr.attach(poll_hz=0)
    try:
        try:
            raise RuntimeError("synthetic crash")
        except RuntimeError:
            exc_type, exc_val, exc_tb = sys.exc_info()
            sys.excepthook(exc_type, exc_val, exc_tb)
    finally:
        smeltr.detach()

    marks = [m["payload"]["label"] for m in fake_daemon.received if m["payload"]["kind"] == "Mark"]
    assert any("uncaught: RuntimeError" in m for m in marks)


def test_atexit_handler_emits_snapshot_and_detaches(fake_daemon):
    smeltr.attach(poll_hz=0)
    from smeltr._shutdown import _atexit_handler

    _atexit_handler()
    snaps = [m for m in fake_daemon.received if m["payload"]["kind"] == "MlxSnapshot"]
    assert len(snaps) >= 1
    from smeltr._api import _client

    assert _client is None


def test_panic_on_queues_systemexit_when_predicate_true(fake_daemon):
    smeltr.attach(poll_hz=0)
    try:
        flag = {"value": False}

        def predicate():
            return flag["value"]

        smeltr.panic_on(predicate, check_every_s=0.02, _exit_via_os=False)
        flag["value"] = True
        time.sleep(0.2)

        from smeltr._shutdown import _drain_panic_for_tests

        with pytest.raises(SystemExit) as ei:
            _drain_panic_for_tests()
        assert ei.value.code == 99

        triggered = [m for m in fake_daemon.received if m["payload"]["kind"] == "MlxPanicTriggered"]
        assert len(triggered) >= 1
    finally:
        smeltr.detach()


def test_sigterm_during_emit_terminates_promptly(fake_daemon):
    # The SIGTERM handler emits a final snapshot. It runs on the main thread,
    # which is usually blocked inside emit() waiting for an Ack — it must not
    # wait for a lock its own thread holds (#239).
    import os
    import signal
    import subprocess

    fake_daemon.ack_delay_s = 0.3
    child = (
        "import smeltr\n"
        "smeltr.attach(poll_hz=0)\n"
        "print('attached', flush=True)\n"
        "while True:\n"
        "    smeltr.mark('tick')\n"
    )
    env = dict(os.environ, SMELTR_MODULES_DISABLE="1")
    p = subprocess.Popen([sys.executable, "-c", child], env=env, stdout=subprocess.PIPE)
    try:
        assert p.stdout is not None
        assert p.stdout.readline().strip() == b"attached"
        time.sleep(0.5)  # well inside a slow emit
        p.send_signal(signal.SIGTERM)
        rc = p.wait(timeout=5)
    finally:
        if p.poll() is None:
            p.kill()
            p.wait()
    assert rc == -signal.SIGTERM


def test_shutdown_handler_runs_while_its_thread_holds_sidecar_locks(fake_daemon):
    # A signal can land while the main thread is inside track() or attach(),
    # holding _tracked_lock or _client_lock; the handler (snapshot + detach)
    # then runs on that same thread and must not wait for them (#239).
    import threading

    from smeltr import _api, _mlx
    from smeltr._shutdown import _atexit_handler

    smeltr.attach(poll_hz=0)

    def handler_inside_held_locks():
        with _mlx._tracked_lock, _api._client_lock:
            _atexit_handler()

    t = threading.Thread(target=handler_inside_held_locks, daemon=True)
    t.start()
    t.join(3.0)
    deadlocked = t.is_alive()
    if deadlocked:
        # A plain Lock may be released by another thread: unblock the stuck
        # one so it unwinds its own locks and teardown does not hang too.
        for lock in (_api._client_lock, _mlx._tracked_lock):
            if isinstance(lock, type(threading.Lock())) and lock.locked():
                lock.release()
        t.join(3.0)
    assert not deadlocked, "shutdown handler deadlocked on a lock its own thread holds"

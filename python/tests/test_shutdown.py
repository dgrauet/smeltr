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

        assert fake_daemon.wait_for(
            lambda ms: any(m["payload"]["kind"] == "MlxPanicTriggered" for m in ms)
        )
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


# ---- #266: attach() must not change the program's SIGTERM semantics ----


def _run_child(body: str, timeout_s: float = 10.0):
    import os
    import subprocess

    env = dict(os.environ, SMELTR_MODULES_DISABLE="1")
    env.pop("SMELTR_AUTOLOAD", None)
    return subprocess.run(
        [sys.executable, "-c", body],
        env=env,
        capture_output=True,
        timeout=timeout_s,
    )


def test_sigterm_chains_the_users_handler(fake_daemon):
    """attach() replaced a graceful SIGTERM handler without calling it: the
    handler never ran and the process died with 143."""
    child = (
        "import os, signal, smeltr\n"
        "seen = []\n"
        "signal.signal(signal.SIGTERM, lambda s, f: seen.append(s))\n"
        "smeltr.attach(poll_hz=0)\n"
        "os.kill(os.getpid(), signal.SIGTERM)\n"
        "print('graceful' if seen == [signal.SIGTERM] else f'lost {seen}', flush=True)\n"
        "smeltr.mark('still-recording')\n"
        "smeltr.detach()\n"
    )
    r = _run_child(child)
    assert r.returncode == 0, r.stderr.decode()
    assert r.stdout.strip() == b"graceful"
    labels = [m["payload"].get("label") for m in fake_daemon.emits()]
    # Still attached after the user's handler kept the process alive.
    assert "still-recording" in labels
    # The sidecar recorded the signal before handing over.
    assert any(m["payload"]["kind"] == "MlxSnapshot" for m in fake_daemon.emits())


def test_sigterm_ignored_by_the_program_stays_ignored(fake_daemon):
    child = (
        "import os, signal, smeltr\n"
        "signal.signal(signal.SIGTERM, signal.SIG_IGN)\n"
        "smeltr.attach(poll_hz=0)\n"
        "os.kill(os.getpid(), signal.SIGTERM)\n"
        "print('alive', flush=True)\n"
        "smeltr.detach()\n"
    )
    r = _run_child(child)
    assert r.returncode == 0, r.stderr.decode()
    assert r.stdout.strip() == b"alive"


def test_default_sigterm_still_terminates_and_flushes(fake_daemon):
    import signal

    child = (
        "import os, signal, smeltr\n"
        "smeltr.attach(poll_hz=0)\n"
        "smeltr.mark('before-term')\n"
        "os.kill(os.getpid(), signal.SIGTERM)\n"
        "import time; time.sleep(5)\n"
        "print('survived', flush=True)\n"
    )
    r = _run_child(child)
    assert r.returncode == -signal.SIGTERM
    assert r.stdout == b""
    labels = [m["payload"].get("label") for m in fake_daemon.emits()]
    assert "before-term" in labels


def test_remove_hooks_leaves_a_handler_installed_after_attach(fake_daemon):
    import signal

    smeltr.attach(poll_hz=0)

    def later(signum, frame):
        pass

    previous = signal.signal(signal.SIGTERM, later)
    try:
        smeltr.detach()
        assert signal.getsignal(signal.SIGTERM) is later
    finally:
        signal.signal(signal.SIGTERM, previous)
        signal.signal(signal.SIGTERM, signal.SIG_DFL)


def test_attach_survives_an_oserror_during_its_hello(fake_daemon, monkeypatch):
    """Only ClientError was caught around the PythonSidecarHello emit: a
    BrokenPipeError escaped attach() with `_client` set and no hooks."""
    from smeltr import _api, _shutdown
    from smeltr._client import _Client

    def broken(self, *a, **k):
        raise BrokenPipeError(32, "Broken pipe")

    monkeypatch.setattr(_Client, "emit", broken)
    smeltr.attach(poll_hz=0)
    try:
        assert _api._client is not None
        assert sys.excepthook is _shutdown._excepthook
    finally:
        smeltr.detach()

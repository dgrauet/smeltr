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

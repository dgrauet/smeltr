import sys

import pytest

import smeltr
from smeltr._client import ClientError


def test_attach_sends_hello_payload(fake_daemon):
    smeltr.attach(poll_hz=0)
    try:
        assert fake_daemon.received, "no events received"
        first = fake_daemon.received[0]
        p = first["payload"]
        assert p["kind"] == "PythonSidecarHello"
        assert p["python_version"].startswith(f"{sys.version_info.major}.")
        assert isinstance(p["argv"], list)
    finally:
        smeltr.detach()


def test_mark_emits_payload(fake_daemon):
    smeltr.attach(poll_hz=0)
    try:
        smeltr.mark("phase: video encode")
    finally:
        smeltr.detach()
    marks = [m for m in fake_daemon.received if m["payload"]["kind"] == "Mark"]
    assert len(marks) == 1
    assert marks[0]["payload"]["label"] == "phase: video encode"


def test_mark_with_fields_emits_structured_fields(fake_daemon):
    smeltr.attach(poll_hz=0)
    try:
        smeltr.mark("checkpoint", step=3, tag="warmup")
    finally:
        smeltr.detach()
    marks = [m for m in fake_daemon.received if m["payload"]["kind"] == "Mark"]
    assert len(marks) == 1
    payload = marks[0]["payload"]
    assert payload["label"] == "checkpoint"
    assert payload["fields"] == {"step": 3, "tag": "warmup"}


def test_session_context_emits_open_and_close(fake_daemon):
    smeltr.attach(poll_hz=0)
    try:
        with smeltr.session("gemma-run"):
            pass
    finally:
        smeltr.detach()
    labels = [m["payload"]["label"] for m in fake_daemon.received if m["payload"]["kind"] == "Mark"]
    assert "session-open: gemma-run" in labels
    assert "session-close: gemma-run" in labels


def test_session_context_emits_close_on_exception(fake_daemon):
    smeltr.attach(poll_hz=0)
    try:
        with pytest.raises(ValueError):
            with smeltr.session("crashy"):
                raise ValueError("boom")
    finally:
        smeltr.detach()
    labels = [m["payload"]["label"] for m in fake_daemon.received if m["payload"]["kind"] == "Mark"]
    assert "session-close: crashy" in labels


def test_mark_without_attach_raises(fake_daemon):
    with pytest.raises(RuntimeError):
        smeltr.mark("oops")


def test_now_returns_monotonic_ns():
    a = smeltr.now()
    b = smeltr.now()
    assert isinstance(a, int)
    assert b >= a


def test_attach_when_daemon_absent_raises(short_tmp_dir, monkeypatch):
    import os

    monkeypatch.setenv("SMELTR_SOCKET", os.path.join(short_tmp_dir, "nope.sock"))
    with pytest.raises(ClientError):
        smeltr.attach(timeout_s=0.5, poll_hz=0)

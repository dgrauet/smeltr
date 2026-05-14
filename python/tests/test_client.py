import pytest

from smeltr._client import ClientError, _Client


def test_connect_handshake(fake_daemon):
    c = _Client()
    c.connect()
    try:
        assert fake_daemon.hello_seen
        assert c.active_session == "00000000000000000000000000000001"
    finally:
        c.close()


def test_emit_records_message(fake_daemon):
    c = _Client()
    c.connect()
    try:
        c.emit({"kind": "Mark", "label": "hello"}, pid=42)
    finally:
        c.close()
    assert len(fake_daemon.received) == 1
    msg = fake_daemon.received[0]
    assert msg["op"] == "Emit"
    assert msg["source"] == "PythonSidecar"
    assert msg["pid"] == 42
    assert msg["payload"] == {"kind": "Mark", "label": "hello"}


def test_emit_without_connect_raises():
    c = _Client()
    with pytest.raises(ClientError):
        c.emit({"kind": "Mark", "label": "x"})


def test_connect_without_server_raises(short_tmp_dir, monkeypatch):
    import os as _os
    monkeypatch.setenv("SMELTR_SOCKET", _os.path.join(short_tmp_dir, "nope.sock"))
    c = _Client()
    with pytest.raises(ClientError):
        c.connect(timeout_s=0.5)

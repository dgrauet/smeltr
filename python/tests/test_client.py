import pytest

from smeltr._client import ClientError, _Client


def test_connect_handshake(fake_daemon):
    c = _Client()
    c.connect()
    try:
        assert fake_daemon.hello_seen
        # The Welcome's resolvable ref, not the ambient UUID bytes (#245).
        assert c.active_session == "ambient1"
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


def test_handshake_with_an_older_daemon_reads_the_uuid_bytes(fake_daemon):
    """Daemons before active_session_ref only send the ambient UUID as 16
    raw bytes: turn them into a ref the CLI resolves, never pass bytes on."""
    fake_daemon.active_session_ref = ""
    c = _Client()
    c.connect()
    try:
        assert c.active_session == "0" * 32
    finally:
        c.close()

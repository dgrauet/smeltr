import pytest

from smeltr._client import DROP_REPORT_LABEL, ClientError, _Client


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


def _labels(msgs):
    return [m["payload"].get("label") for m in msgs if m["payload"]["kind"] == "Mark"]


def test_emit_never_waits_for_a_stalled_daemon(fake_daemon):
    """#266: every emit was a write + wait-for-Ack round trip, so a stalled
    daemon stalled the user's forward (22 s for three module calls)."""
    import time

    c = _Client(queue_max=16)
    c.connect()
    try:
        fake_daemon.gate.clear()
        t = time.perf_counter()
        for i in range(500):
            c.emit({"kind": "Mark", "label": f"m{i}"}, pid=1)
        elapsed = time.perf_counter() - t
        assert elapsed < 0.25, f"500 emits took {elapsed:.3f} s against a stalled daemon"
        assert c.dropped > 0
    finally:
        fake_daemon.gate.set()
        c.close()


def test_drops_are_reported_to_the_daemon(fake_daemon):
    c = _Client(queue_max=8)
    c.connect()
    try:
        fake_daemon.gate.clear()
        for i in range(100):
            c.emit({"kind": "Mark", "label": f"m{i}"}, pid=1)
        dropped = c.dropped
        assert dropped > 0
    finally:
        fake_daemon.gate.set()
        c.close()
    msgs = fake_daemon.emits()
    delivered = [m for m in msgs if (m["payload"].get("label") or "").startswith("m")]
    reports = [m for m in msgs if m["payload"].get("label") == DROP_REPORT_LABEL]
    assert reports, _labels(msgs)
    total = reports[-1]["payload"]["fields"]["dropped"]
    assert total >= dropped
    assert len(delivered) + total == 100


def test_emits_are_stamped_where_they_happen(fake_daemon):
    import time

    c = _Client()
    c.connect()
    before = time.clock_gettime_ns(time.CLOCK_UPTIME_RAW)
    c.emit({"kind": "Mark", "label": "x"}, pid=1)
    after = time.clock_gettime_ns(time.CLOCK_UPTIME_RAW)
    c.close()
    stamp = fake_daemon.emits()[0]["at_uptime_raw_ns"]
    assert before <= stamp <= after


def test_close_flushes_queued_events_in_order(fake_daemon):
    c = _Client()
    c.connect()
    for i in range(300):
        c.emit({"kind": "Mark", "label": f"m{i}"}, pid=1)
    c.close()
    assert _labels(fake_daemon.emits()) == [f"m{i}" for i in range(300)]


def test_close_is_bounded_when_the_daemon_is_stalled(fake_daemon):
    import time

    c = _Client()
    c.connect()
    fake_daemon.gate.clear()
    try:
        for i in range(50):
            c.emit({"kind": "Mark", "label": f"m{i}"}, pid=1)
        t = time.perf_counter()
        c.close(flush_timeout_s=0.3)
        assert time.perf_counter() - t < 1.0
        # Unblocked, not left waiting on the stalled daemon.
        assert c._sender is not None and not c._sender.is_alive()
    finally:
        fake_daemon.gate.set()


def test_reconnects_after_the_daemon_restarts(fake_daemon):
    from tests.conftest import FakeDaemon

    c = _Client()
    c.connect()
    try:
        c.emit({"kind": "Mark", "label": "before"}, pid=1)
        assert fake_daemon.wait_for(lambda m: _labels(m) == ["before"])
        fake_daemon.stop()
        # Emits while the daemon is gone must not raise.
        c.emit({"kind": "Mark", "label": "while-down"}, pid=1)
        restarted = FakeDaemon(fake_daemon.sock_path)
        restarted.start()
        try:
            c.emit({"kind": "Mark", "label": "after"}, pid=1)
            assert restarted.wait_for(lambda m: "after" in _labels(m), timeout_s=8.0)
            assert restarted.hello_seen
        finally:
            restarted.stop()
    finally:
        c.close(flush_timeout_s=0.2)


def test_a_timed_out_exchange_does_not_desync_the_stream(fake_daemon):
    """A send/recv timeout mid-exchange left the stream desynchronised for
    good: the next Ack read would consume a stale reply. The sender now
    drops the connection and starts a fresh one."""
    import time

    c = _Client(io_timeout_s=0.2)
    c.connect()
    try:
        fake_daemon.gate.clear()
        # Enough bytes to fill the socket buffers: the sender's write times
        # out mid-frame, and its Ack read times out.
        for i in range(200):
            c.emit({"kind": "Mark", "label": f"stalled{i}-" + "x" * 2048}, pid=1)
        time.sleep(0.6)
        fake_daemon.gate.set()
        c.emit({"kind": "Mark", "label": "later"}, pid=1)
        assert fake_daemon.wait_for(lambda m: "later" in _labels(m), timeout_s=8.0)
    finally:
        c.close()


def test_closed_clients_release_their_file_descriptors(fake_daemon):
    import gc
    import os

    def open_fds():
        return len(os.listdir("/dev/fd"))

    c = _Client()
    c.connect()
    c.close()
    before = open_fds()
    for _ in range(20):
        c = _Client()
        c.connect()
        c.emit({"kind": "Mark", "label": "x"}, pid=1)
        c.close()
        del c
        gc.collect()
    assert open_fds() <= before + 2


def test_socket_does_not_raise_sigpipe(fake_daemon):
    """A program that restored SIGPIPE=SIG_DFL died (exit 141) when the
    daemon went away mid-write."""
    import socket
    import sys

    if sys.platform != "darwin":
        pytest.skip("SO_NOSIGPIPE is a Darwin socket option")
    c = _Client()
    c.connect()
    try:
        assert c._sock is not None
        assert c._sock.getsockopt(socket.SOL_SOCKET, 0x1022) != 0
    finally:
        c.close()


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

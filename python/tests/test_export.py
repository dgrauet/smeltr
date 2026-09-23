"""Tests for smeltr.export() — Python wrapper around `smeltr export` CLI."""

from __future__ import annotations

import subprocess
from unittest.mock import MagicMock, patch

import pytest

import smeltr


@patch("smeltr._api._client", None)
def test_export_requires_attach_when_session_is_none():
    """Without attach() and without explicit session, raise.
    Explicitly pins _client=None so the test is isolated from any prior
    attach() call leftover in the same pytest process.
    """
    with pytest.raises(RuntimeError, match="attach"):
        smeltr.export("/tmp/whatever.json")


@patch("shutil.which", return_value=None)
def test_export_raises_when_cli_not_on_path(_which):
    with pytest.raises(RuntimeError, match="smeltr CLI not found"):
        smeltr.export("/tmp/whatever.json", session="some-session-id")


@patch("shutil.which", return_value="/usr/local/bin/smeltr")
@patch("subprocess.run")
def test_export_explicit_session_shells_out(run_mock, _which):
    run_mock.return_value = MagicMock(returncode=0, stdout=b"", stderr=b"")
    smeltr.export("/tmp/trace.json", format="chrome-trace", session="abc123")
    run_mock.assert_called_once()
    cmd = run_mock.call_args[0][0]
    assert cmd[0] == "/usr/local/bin/smeltr"
    assert "export" in cmd
    assert "abc123" in cmd
    assert "/tmp/trace.json" in cmd
    assert "chrome-trace" in cmd


@patch("shutil.which", return_value="/usr/local/bin/smeltr")
@patch("subprocess.run")
def test_export_default_format_is_chrome_trace(run_mock, _which):
    run_mock.return_value = MagicMock(returncode=0, stdout=b"", stderr=b"")
    smeltr.export("/tmp/trace.json", session="abc123")
    cmd = run_mock.call_args[0][0]
    assert "chrome-trace" in cmd


@patch("shutil.which", return_value="/usr/local/bin/smeltr")
@patch("subprocess.run")
def test_export_raises_on_nonzero_exit(run_mock, _which):
    run_mock.side_effect = subprocess.CalledProcessError(
        returncode=1, cmd=["smeltr", "export"], output=b"", stderr=b"boom"
    )
    with pytest.raises(RuntimeError, match="boom"):
        smeltr.export("/tmp/trace.json", session="abc123")


@patch("shutil.which", return_value="/usr/local/bin/smeltr")
@patch("subprocess.run")
def test_export_defaults_to_the_recording_of_this_process(
    run_mock, _which, fake_daemon, monkeypatch
):
    """#245: export() without a session exported the daemon's ambient
    session, passed as raw UUID bytes. Under `smeltr record` the process
    carries SMELTR_SCOPE_TOKEN; the daemon names that recording, asked at
    export time (the recording may register after attach())."""
    run_mock.return_value = MagicMock(returncode=0, stdout=b"", stderr=b"")
    monkeypatch.setenv("SMELTR_SCOPE_TOKEN", "tok-123")
    smeltr.attach(poll_hz=0)
    try:
        fake_daemon.active_session_ref = "rec12345"
        smeltr.export("/tmp/trace.json")
    finally:
        smeltr.detach()
    cmd = run_mock.call_args[0][0]
    assert cmd[2] == "rec12345", cmd
    assert fake_daemon.hello_tokens[-1] == "tok-123"

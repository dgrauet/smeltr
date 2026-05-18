"""Tests for smeltr.export() — Python wrapper around `smeltr export` CLI."""

from __future__ import annotations

import subprocess
from unittest.mock import MagicMock, patch

import pytest

import smeltr


def test_export_requires_attach_when_session_is_none():
    """Without attach() and without explicit session, raise."""
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

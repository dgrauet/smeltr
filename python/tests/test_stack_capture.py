"""Tests for the SMELTR_STACK_CAPTURE env-gated stack capture helper."""

from __future__ import annotations

import os
from unittest.mock import patch

from smeltr._mlx import _capture_stack


def test_capture_returns_empty_when_env_unset(monkeypatch):
    monkeypatch.delenv("SMELTR_STACK_CAPTURE", raising=False)
    assert _capture_stack() == []


@patch.dict(os.environ, {"SMELTR_STACK_CAPTURE": "1"})
def test_capture_returns_frames_when_enabled():
    def inner_helper():
        return _capture_stack(depth=3)

    frames = inner_helper()
    assert isinstance(frames, list)
    assert len(frames) >= 1
    assert isinstance(frames[0], dict)
    assert "filename" in frames[0]
    assert "lineno" in frames[0]
    assert "funcname" in frames[0]
    # The top non-smeltr frame should be this test file, not _mlx.py.
    assert frames[0]["filename"].endswith("test_stack_capture.py")
    assert frames[0]["funcname"] == "inner_helper"


@patch.dict(os.environ, {"SMELTR_STACK_CAPTURE": "1"})
def test_capture_caps_at_requested_depth():
    frames = _capture_stack(depth=1)
    assert len(frames) == 1


@patch.dict(os.environ, {"SMELTR_STACK_CAPTURE": "0"})
def test_capture_disabled_when_env_is_not_one():
    assert _capture_stack() == []

"""Verifies the Python sidecar propagates SMELTR_SCOPE_TOKEN into every Emit."""

from __future__ import annotations

from smeltr._proto import emit_msg


def test_emit_msg_omits_scope_token_when_none() -> None:
    msg = emit_msg("PythonSidecar", 4242, {"kind": "Mark", "label": "x"})
    assert "scope_token" not in msg


def test_emit_msg_includes_scope_token_when_set() -> None:
    msg = emit_msg(
        "PythonSidecar",
        4242,
        {"kind": "Mark", "label": "x"},
        scope_token="tok-123",
    )
    assert msg["scope_token"] == "tok-123"

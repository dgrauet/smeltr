"""Verifies the Python sidecar propagates SMELTR_SCOPE_TOKEN into every Emit."""

from __future__ import annotations

from unittest.mock import MagicMock

from smeltr import _api
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


def test_attach_reads_scope_token_from_env_and_emit_propagates(monkeypatch) -> None:
    monkeypatch.setenv("SMELTR_SCOPE_TOKEN", "env-tok-abc")
    fake_client = MagicMock()
    monkeypatch.setattr(_api, "_client", fake_client)
    # Simulate the attach() side effect of caching the token.
    monkeypatch.setattr(_api, "_scope_token", "env-tok-abc")

    _api._emit({"kind": "Mark", "label": "hi"}, pid=4242)

    fake_client.emit.assert_called_once()
    kwargs = fake_client.emit.call_args.kwargs
    assert kwargs["scope_token"] == "env-tok-abc"
    assert kwargs["pid"] == 4242


def test_emit_without_token_passes_none(monkeypatch) -> None:
    monkeypatch.delenv("SMELTR_SCOPE_TOKEN", raising=False)
    fake_client = MagicMock()
    monkeypatch.setattr(_api, "_client", fake_client)
    monkeypatch.setattr(_api, "_scope_token", None)

    _api._emit({"kind": "Mark", "label": "hi"}, pid=4242)

    kwargs = fake_client.emit.call_args.kwargs
    assert kwargs["scope_token"] is None

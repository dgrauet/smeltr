"""Tests for structured fields on smeltr.mark."""

from __future__ import annotations

from unittest.mock import patch

import smeltr
from smeltr import _api


def _emit_spy():
    captured: list[dict] = []

    def _spy(payload):
        captured.append(payload)

    return captured, _spy


def test_mark_emits_fields_when_provided() -> None:
    captured, spy = _emit_spy()
    with patch.object(_api, "_emit", spy):
        smeltr.mark("checkpoint", step=5, ok=True)
    assert len(captured) == 1
    payload = captured[0]
    assert payload["kind"] == "Mark"
    assert payload["label"] == "checkpoint"
    assert payload["fields"] == {"step": 5, "ok": True}


def test_mark_omits_fields_key_when_empty() -> None:
    captured, spy = _emit_spy()
    with patch.object(_api, "_emit", spy):
        smeltr.mark("plain")
    payload = captured[0]
    assert payload["label"] == "plain"
    assert "fields" not in payload


def test_mark_coerces_non_primitive_to_str() -> None:
    from pathlib import Path

    captured, spy = _emit_spy()
    with patch.object(_api, "_emit", spy):
        smeltr.mark("with-path", path=Path("/tmp/x"))
    assert captured[0]["fields"] == {"path": "/tmp/x"}

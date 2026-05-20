"""Tests for #43: smeltr.scope(name, **fields) structured metadata."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import patch

import smeltr
from smeltr import _modules


def _emits():
    """Capture every _emit() call during a `with` block."""
    captured: list[dict] = []

    def _spy(payload):
        captured.append(payload)

    return captured, _spy


def test_scope_emits_fields_when_provided() -> None:
    captured, spy = _emits()
    with patch.object(_modules, "_emit", spy):
        with smeltr.scope("denoise.step", step=3, label="cond"):
            pass
    enters = [c for c in captured if c.get("kind") == "ModuleEntered"]
    assert len(enters) == 1
    assert enters[0]["fields"] == {"step": 3, "label": "cond"}


def test_scope_omits_fields_key_when_empty() -> None:
    captured, spy = _emits()
    with patch.object(_modules, "_emit", spy):
        with smeltr.scope("plain"):
            pass
    enters = [c for c in captured if c.get("kind") == "ModuleEntered"]
    assert len(enters) == 1
    assert "fields" not in enters[0]


def test_scope_decorator_passes_fields() -> None:
    captured, spy = _emits()

    @smeltr.scope("forward", layer=3)
    def f(x):
        return x + 1

    with patch.object(_modules, "_emit", spy):
        assert f(10) == 11
    enters = [c for c in captured if c.get("kind") == "ModuleEntered"]
    assert len(enters) == 1
    assert enters[0]["fields"] == {"layer": 3}


def test_scope_coerces_non_primitive_to_str() -> None:
    captured, spy = _emits()
    with patch.object(_modules, "_emit", spy):
        with smeltr.scope("pathy", out=Path("/tmp/x")):
            pass
    enters = [c for c in captured if c.get("kind") == "ModuleEntered"]
    assert len(enters) == 1
    assert enters[0]["fields"] == {"out": "/tmp/x"}

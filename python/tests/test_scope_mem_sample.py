"""Tests for #47: synchronous MTL memory reads on scope enter/exit."""

from __future__ import annotations

from unittest.mock import patch

import smeltr
from smeltr import _mlx, _modules


def _emits():
    captured: list[dict] = []

    def _spy(payload):
        captured.append(payload)

    return captured, _spy


def test_scope_emits_mem_sample_on_enter_and_exit(monkeypatch) -> None:
    monkeypatch.delenv("SMELTR_SCOPE_MEM_SAMPLE", raising=False)
    captured, spy = _emits()
    with (
        patch.object(_modules, "_emit", spy),
        patch.object(_mlx, "read_device_memory_bytes", lambda: (123_456_789, 17_179_869_184)),
    ):
        with smeltr.scope("foo"):
            pass
    kinds = [c.get("kind") for c in captured]
    # Order: ModuleEntered, sample(scope_enter), sample(scope_exit), ModuleReturned.
    assert kinds == [
        "ModuleEntered",
        "MetalDeviceMemSample",
        "MetalDeviceMemSample",
        "ModuleReturned",
    ], kinds
    enter_sample = captured[1]
    exit_sample = captured[2]
    assert enter_sample["at_event"] == "scope_enter"
    assert enter_sample["allocated_bytes"] == 123_456_789
    assert enter_sample["recommended_max_bytes"] == 17_179_869_184
    assert exit_sample["at_event"] == "scope_exit"


def test_scope_skips_mem_sample_when_mx_metal_unavailable(monkeypatch) -> None:
    monkeypatch.delenv("SMELTR_SCOPE_MEM_SAMPLE", raising=False)
    captured, spy = _emits()
    with (
        patch.object(_modules, "_emit", spy),
        patch.object(_mlx, "read_device_memory_bytes", lambda: None),
    ):
        with smeltr.scope("foo"):
            pass
    kinds = [c.get("kind") for c in captured]
    assert kinds == ["ModuleEntered", "ModuleReturned"], kinds


def test_scope_mem_sample_disabled_via_env(monkeypatch) -> None:
    monkeypatch.setenv("SMELTR_SCOPE_MEM_SAMPLE", "0")
    captured, spy = _emits()
    with (
        patch.object(_modules, "_emit", spy),
        patch.object(_mlx, "read_device_memory_bytes", lambda: (1, 2)),
    ):
        with smeltr.scope("foo"):
            pass
    kinds = [c.get("kind") for c in captured]
    assert kinds == ["ModuleEntered", "ModuleReturned"], kinds


def test_auto_module_wrap_does_not_emit_mem_sample(monkeypatch) -> None:
    """Only user-defined smeltr.scope() brackets samples.

    Auto-wrapped mlx.nn.Module.__call__ must NOT incur the MTL read cost
    on every Module call.
    """
    monkeypatch.delenv("SMELTR_SCOPE_MEM_SAMPLE", raising=False)
    captured, spy = _emits()
    with (
        patch.object(_modules, "_emit", spy),
        patch.object(_mlx, "read_device_memory_bytes", lambda: (1, 2)),
    ):
        cid = _modules._push("MyModule", "MyModule", id_of=42)
        _modules._pop(cid)
    kinds = [c.get("kind") for c in captured]
    assert kinds == ["ModuleEntered", "ModuleReturned"], kinds

"""Tests for smeltr._modules (thread-local module-call stack + monkey-patch)."""

from __future__ import annotations

import threading
from typing import Any
from unittest.mock import patch

import pytest

from smeltr import _modules


def _fake_emit_recorder() -> tuple[list[dict[str, Any]], Any]:
    events: list[dict[str, Any]] = []

    def fake(payload: dict[str, Any]) -> None:
        events.append(payload)

    return events, fake


def test_stack_starts_empty():
    _modules._reset_for_tests()
    assert _modules._current_stack() == []


def test_push_pop_round_trip():
    _modules._reset_for_tests()
    cid = _modules._push("Foo", "Foo", id_of=42)
    assert _modules._current_stack() == [cid]
    _modules._pop(cid)
    assert _modules._current_stack() == []


def test_install_idempotent():
    _modules._reset_for_tests()
    pytest.importorskip("mlx.nn")
    _modules.install()
    _modules.install()
    import mlx.nn as nn

    assert getattr(nn.Module.__call__, "_smeltr_wrapped", False) is True


def test_call_emits_entered_and_returned():
    _modules._reset_for_tests()
    pytest.importorskip("mlx.nn")
    import mlx.core as mx
    import mlx.nn as nn

    events, fake = _fake_emit_recorder()
    with patch.object(_modules, "_emit", fake):
        _modules.install()
        layer = nn.Linear(2, 2)
        _ = layer(mx.zeros((1, 2)))

    kinds = [e["kind"] for e in events]
    assert "ModuleEntered" in kinds
    assert "ModuleReturned" in kinds
    entered = next(e for e in events if e["kind"] == "ModuleEntered")
    assert entered["class_name"] == "Linear"
    assert entered["depth"] == 0
    assert entered["parent_call_id"] is None


def test_nested_calls_track_parent():
    _modules._reset_for_tests()
    pytest.importorskip("mlx.nn")
    import mlx.core as mx
    import mlx.nn as nn

    class Outer(nn.Module):
        def __init__(self):
            super().__init__()
            self.inner = nn.Linear(2, 2)

        def __call__(self, x):
            return self.inner(x)

    events, fake = _fake_emit_recorder()
    with patch.object(_modules, "_emit", fake):
        _modules.install()
        _ = Outer()(mx.zeros((1, 2)))

    entered = [e for e in events if e["kind"] == "ModuleEntered"]
    assert len(entered) >= 2
    outer, inner = entered[0], entered[1]
    assert outer["parent_call_id"] is None
    assert outer["depth"] == 0
    assert inner["parent_call_id"] == outer["module_call_id"]
    assert inner["depth"] == 1


def test_exception_in_forward_pops_stack():
    _modules._reset_for_tests()
    pytest.importorskip("mlx.nn")
    import mlx.nn as nn

    class Boom(nn.Module):
        def __call__(self, x):
            raise RuntimeError("boom")

    events, fake = _fake_emit_recorder()
    with patch.object(_modules, "_emit", fake):
        _modules.install()
        with pytest.raises(RuntimeError):
            Boom()(None)

    assert _modules._current_stack() == []
    assert "ModuleReturned" in [e["kind"] for e in events]


def test_threads_are_isolated():
    _modules._reset_for_tests()
    pytest.importorskip("mlx.nn")

    seen: list[list[int]] = []
    barrier = threading.Barrier(2)

    def worker():
        cid = _modules._push("T", "T", id_of=1)
        barrier.wait()
        seen.append(list(_modules._current_stack()))
        _modules._pop(cid)

    threads = [threading.Thread(target=worker) for _ in range(2)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert all(len(s) == 1 for s in seen)


def test_disable_env_var_makes_install_noop(monkeypatch):
    _modules._reset_for_tests()
    monkeypatch.setenv("SMELTR_MODULES_DISABLE", "1")
    pytest.importorskip("mlx.nn")
    import mlx.nn as nn

    _modules.install()
    assert getattr(nn.Module.__call__, "_smeltr_wrapped", False) is False


def test_install_without_mlx_is_noop(monkeypatch):
    _modules._reset_for_tests()
    import builtins as _builtins

    original_import = _builtins.__import__

    def fake_import(name, *args, **kwargs):
        if name.startswith("mlx"):
            raise ImportError("simulated missing mlx")
        return original_import(name, *args, **kwargs)

    monkeypatch.setattr(_builtins, "__import__", fake_import)
    _modules.install()
    assert _modules._current_stack() == []


def test_uninstall_restores_original_call():
    """Verify uninstall() actually removes the sentinel and any wrappers."""
    _modules._reset_for_tests()
    pytest.importorskip("mlx.nn")
    import mlx.nn as nn

    before_module_call = nn.Module.__dict__.get("__call__")
    before_linear_call = nn.Linear.__dict__.get("__call__")
    _modules.install()
    assert getattr(nn.Module.__call__, "_smeltr_wrapped", False) is True
    _modules.uninstall()
    after_module_call = nn.Module.__dict__.get("__call__")
    after_linear_call = nn.Linear.__dict__.get("__call__")
    assert after_module_call == before_module_call
    assert after_linear_call == before_linear_call
    assert getattr(nn.Module.__call__, "_smeltr_wrapped", False) is False

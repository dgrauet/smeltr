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

    assert getattr(nn.Linear.__call__, "_smeltr_wrapped", False) is True


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
    assert getattr(nn.Linear.__call__, "_smeltr_wrapped", False) is False


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
    assert getattr(nn.Linear.__call__, "_smeltr_wrapped", False) is True
    _modules.uninstall()
    after_module_call = nn.Module.__dict__.get("__call__")
    after_linear_call = nn.Linear.__dict__.get("__call__")
    assert after_module_call == before_module_call
    assert after_linear_call == before_linear_call
    assert getattr(nn.Linear.__call__, "_smeltr_wrapped", False) is False


def test_mlx_eval_payload_includes_module_stack():
    _modules._reset_for_tests()
    pytest.importorskip("mlx.core")
    pytest.importorskip("mlx.nn")
    import mlx.core as mx
    import mlx.nn as nn

    from smeltr import _mlx

    class EvalInside(nn.Module):
        def __init__(self):
            super().__init__()
            self.lin = nn.Linear(2, 2)

        def __call__(self, x):
            y = self.lin(x)
            mx.eval(y)
            return y

    events, fake = _fake_emit_recorder()
    with patch.object(_modules, "_emit", fake), patch("smeltr._mlx._emit", fake):
        _modules.install()
        _mlx.decorate_eval()
        try:
            EvalInside()(mx.zeros((1, 2)))
        finally:
            _mlx._undecorate_eval_for_tests()
            _modules.uninstall()

    entered = [e for e in events if e["kind"] == "MlxEvalEntered"]
    assert len(entered) >= 1
    for ev in entered:
        assert "module_stack" in ev
        assert isinstance(ev["module_stack"], list)
    assert any(len(ev["module_stack"]) > 0 for ev in entered), (
        f"expected at least one MlxEvalEntered with a non-empty module_stack; "
        f"got: {[ev['module_stack'] for ev in entered]}"
    )


# ---- #266: the wrapper must never change or break the user's forward ----


def _entered(events):
    return [e for e in events if e["kind"] == "ModuleEntered"]


def test_array_valued_name_attribute_does_not_break_the_forward():
    """`getattr(module, "name", None) or cls` called bool() on an mx.array:
    ValueError raised inside the user's forward (#266)."""
    _modules._reset_for_tests()
    pytest.importorskip("mlx.nn")
    import mlx.core as mx
    import mlx.nn as nn

    class Named(nn.Module):
        def __init__(self):
            super().__init__()
            self.name = mx.array([1.0, 2.0])

        def __call__(self, x):
            return x + 1

    events, fake = _fake_emit_recorder()
    with patch.object(_modules, "_emit", fake):
        _modules.install()
        try:
            out = Named()(mx.zeros((2,)))
        finally:
            _modules.uninstall()
    assert out.tolist() == [1.0, 1.0]
    assert _entered(events)[0]["qualname"] == "Named"


def test_only_a_non_empty_str_name_labels_the_qualname():
    class Plain:
        pass

    m = Plain()
    assert _modules._qualname_for(m) == "Plain"
    m.name = 42
    assert _modules._qualname_for(m) == "Plain"
    m.name = ""
    assert _modules._qualname_for(m) == "Plain"
    m.name = "encoder"
    assert _modules._qualname_for(m) == "Plain:encoder"


def test_bookkeeping_failure_never_reaches_the_user_call():
    _modules._reset_for_tests()
    pytest.importorskip("mlx.nn")
    import mlx.nn as nn

    class Echo(nn.Module):
        def __call__(self, x):
            return x

    def boom(*_a, **_k):
        raise RuntimeError("bookkeeping broke")

    _modules.install()
    try:
        with patch.object(_modules, "_push", boom), patch.object(_modules, "_pop", boom):
            assert Echo()(7) == 7
    finally:
        _modules.uninstall()


def test_user_exception_propagates_unchanged():
    _modules._reset_for_tests()
    pytest.importorskip("mlx.nn")
    import mlx.nn as nn

    class Fails(nn.Module):
        def __call__(self, x):
            raise KeyError("user error")

    _modules.install()
    try:
        with pytest.raises(KeyError, match="user error"):
            Fails()(1)
    finally:
        _modules.uninstall()


def test_wrapper_preserves_the_call_signature():
    """mlx_lm.utils.does_model_support_input_embeddings() checks
    `'input_embeddings' in inspect.signature(model.__call__).parameters`;
    a `(*args, **kwargs)` wrapper turned it False under `smeltr record`."""
    import inspect

    _modules._reset_for_tests()
    pytest.importorskip("mlx.nn")
    import mlx.nn as nn

    class Model(nn.Module):
        def __call__(self, inputs, cache=None, input_embeddings=None):
            """Forward docstring."""
            return inputs

    _modules.install()
    try:
        params = inspect.signature(Model().__call__).parameters
        assert list(params) == ["inputs", "cache", "input_embeddings"]
        assert Model.__call__.__doc__ == "Forward docstring."
        assert Model.__call__.__name__ == "__call__"
        assert getattr(Model.__call__, "_smeltr_wrapped", False) is True
    finally:
        _modules.uninstall()


def test_module_without_call_stays_non_callable():
    _modules._reset_for_tests()
    pytest.importorskip("mlx.nn")
    import mlx.nn as nn

    class Container(nn.Module):
        pass

    _modules.install()
    try:
        assert not callable(Container())
        assert "__call__" not in nn.Module.__dict__
    finally:
        _modules.uninstall()


def test_super_call_records_the_scope_once():
    _modules._reset_for_tests()
    pytest.importorskip("mlx.nn")
    import mlx.nn as nn

    class Base(nn.Module):
        def __call__(self, x):
            return x + 1

    class Child(Base):
        def __call__(self, x):
            return super().__call__(x) * 2

    class Inherits(Base):
        pass

    events, fake = _fake_emit_recorder()
    with patch.object(_modules, "_emit", fake):
        _modules.install()
        try:
            assert Child()(1) == 4
            assert Inherits()(1) == 2
        finally:
            _modules.uninstall()
    entered = _entered(events)
    assert [e["class_name"] for e in entered] == ["Child", "Inherits"]
    returned = [e for e in events if e["kind"] == "ModuleReturned"]
    assert len(returned) == 2
    assert _modules._current_stack() == []

"""Tests for smeltr.scope context manager and decorator."""

from __future__ import annotations

import pytest

from smeltr import _modules
from smeltr._scopes import scope


@pytest.fixture(autouse=True)
def _reset_modules_state():
    _modules._reset_for_tests()
    yield
    _modules._reset_for_tests()


def test_scope_pushes_and_pops_frame_on_tls_stack():
    assert _modules._stack() == []
    with scope("denoise.pass:cond"):
        stack = _modules._stack()
        assert len(stack) == 1
        assert stack[0]["qualname"] == "denoise.pass:cond"
        assert stack[0]["class_name"] == "Scope"
    assert _modules._stack() == []


def test_nested_scopes_have_parent_relationship():
    with scope("outer"):
        outer_cid = _modules._stack()[-1]["module_call_id"]
        with scope("inner"):
            stack = _modules._stack()
            assert [f["qualname"] for f in stack] == ["outer", "inner"]
            assert stack[-1]["depth"] == 1
        assert [f["qualname"] for f in _modules._stack()] == ["outer"]
        assert _modules._stack()[-1]["module_call_id"] == outer_cid
    assert _modules._stack() == []


def test_decorator_form_wraps_function():
    @scope("forward")
    def fn(x: int) -> int:
        assert [f["qualname"] for f in _modules._stack()] == ["forward"]
        return x * 2

    assert fn(3) == 6
    assert _modules._stack() == []


def test_scope_pops_even_when_body_raises():
    with pytest.raises(RuntimeError, match="boom"):
        with scope("crashy"):
            assert _modules._stack()[-1]["qualname"] == "crashy"
            raise RuntimeError("boom")
    assert _modules._stack() == []


def test_decorator_preserves_function_metadata():
    @scope("named")
    def my_func(x: int, y: int = 5) -> int:
        return x + y

    assert my_func.__name__ == "my_func"
    assert my_func(2) == 7


def test_scope_is_exported_from_top_level_smeltr():
    import smeltr

    assert hasattr(smeltr, "scope")
    with smeltr.scope("via-top-level"):
        assert _modules._stack()[-1]["qualname"] == "via-top-level"


def test_decorator_rejects_async_function():
    async def async_fn():
        pass

    with pytest.raises(TypeError, match="does not support async functions"):
        scope("bad")(async_fn)


def test_decorator_rejects_generator_function():
    def gen_fn():
        yield 1

    with pytest.raises(TypeError, match="does not support generator functions"):
        scope("bad")(gen_fn)


def test_decorator_rejects_async_generator():
    async def async_gen():
        yield 1

    with pytest.raises(TypeError, match="does not support async functions"):
        scope("bad")(async_gen)


# ---- capture Metal par scope nommé (#216) ----


@pytest.fixture
def _capture_env(monkeypatch, tmp_path):
    """Arme la capture sur le scope `cible`, dans un chemin jetable."""
    from smeltr import _scopes

    path = tmp_path / "run.gputrace"
    monkeypatch.setenv("SMELTR_GPUTRACE_SCOPE", "cible")
    monkeypatch.setenv("SMELTR_GPUTRACE_PATH", str(path))
    _scopes._reset_capture_for_tests()
    yield path
    _scopes._reset_capture_for_tests()


class _FakeMetal:
    """Double de `mx.metal`, pour ne pas déclencher de vraie capture."""

    def __init__(self):
        self.started = []
        self.stopped = 0

    def start_capture(self, path):
        self.started.append(path)

    def stop_capture(self):
        self.stopped += 1


def test_capture_starts_and_stops_on_the_named_scope(_capture_env, monkeypatch):
    from smeltr import _scopes

    fake = _FakeMetal()
    monkeypatch.setattr(_scopes, "_metal_capture_api", lambda: fake)

    with scope("cible"):
        assert fake.started == [str(_capture_env)]
        assert fake.stopped == 0
    assert fake.stopped == 1


def test_capture_ignores_other_scopes(_capture_env, monkeypatch):
    from smeltr import _scopes

    fake = _FakeMetal()
    monkeypatch.setattr(_scopes, "_metal_capture_api", lambda: fake)

    with scope("autre"):
        pass
    assert fake.started == []
    assert fake.stopped == 0


def test_capture_fires_once_even_if_the_scope_repeats(_capture_env, monkeypatch):
    """Un scope dans une boucle ne doit pas relancer la capture à chaque tour."""
    from smeltr import _scopes

    fake = _FakeMetal()
    monkeypatch.setattr(_scopes, "_metal_capture_api", lambda: fake)

    for _ in range(3):
        with scope("cible"):
            pass
    assert len(fake.started) == 1
    assert fake.stopped == 1


def test_capture_stops_even_when_the_body_raises(_capture_env, monkeypatch):
    from smeltr import _scopes

    fake = _FakeMetal()
    monkeypatch.setattr(_scopes, "_metal_capture_api", lambda: fake)

    with pytest.raises(ValueError):
        with scope("cible"):
            raise ValueError("boom")
    assert fake.stopped == 1


def test_a_failing_capture_never_breaks_user_code(_capture_env, monkeypatch):
    """L'observabilité ne doit jamais casser la mesure en cours."""
    from smeltr import _scopes

    class _Broken:
        def start_capture(self, path):
            raise RuntimeError("MTL_CAPTURE_ENABLED absent")

        def stop_capture(self):
            raise RuntimeError("rien à arrêter")

    monkeypatch.setattr(_scopes, "_metal_capture_api", lambda: _Broken())

    executed = []
    with scope("cible"):
        executed.append(True)
    assert executed == [True]


def test_no_capture_without_the_env_vars(monkeypatch):
    from smeltr import _scopes

    monkeypatch.delenv("SMELTR_GPUTRACE_SCOPE", raising=False)
    monkeypatch.delenv("SMELTR_GPUTRACE_PATH", raising=False)
    _scopes._reset_capture_for_tests()

    fake = _FakeMetal()
    monkeypatch.setattr(_scopes, "_metal_capture_api", lambda: fake)

    with scope("cible"):
        pass
    assert fake.started == []

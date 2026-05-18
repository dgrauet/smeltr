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

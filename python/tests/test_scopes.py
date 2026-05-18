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

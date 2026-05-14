"""Smoke tests - verify the package installs and exposes basic attributes."""

import smeltr


def test_version_is_string():
    assert isinstance(smeltr.__version__, str)
    assert smeltr.__version__  # non-empty


def test_public_api_placeholders_exist():
    for name in (
        "attach",
        "detach",
        "session",
        "mark",
        "now",
        "snapshot",
        "decorate_eval",
        "panic_on",
    ):
        assert hasattr(smeltr, name), f"missing public symbol: {name}"

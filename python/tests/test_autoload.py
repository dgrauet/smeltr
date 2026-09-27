"""#266: the `.pth` must cost nothing when SMELTR_AUTOLOAD is unset, and
autoload must not import mlx into a process that never uses it —
`import mlx.core` allocates a Metal heap, which made a Python launcher look
like a Metal process under `smeltr record`."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import sysconfig
from pathlib import Path

import pytest

PYTHON_DIR = Path(__file__).resolve().parents[1]
SRC = PYTHON_DIR / "src"

# Runs with -S, then processes the repository's .pth the way site.py does,
# so the test exercises the checked-in file, not an installed copy.
_PRELUDE = f"""
import json, site, sys
sys.path[:0] = [{str(SRC)!r}, {sysconfig.get_paths()["purelib"]!r}]
site.addpackage({str(PYTHON_DIR)!r}, "smeltr-autoload.pth", set())
def loaded(prefix):
    return sorted(m for m in sys.modules if m == prefix or m.startswith(prefix + "."))
"""


def _run(body: str, env_extra: dict[str, str]) -> dict:
    env = {k: v for k, v in os.environ.items() if not k.startswith("SMELTR_")}
    env.update(env_extra)
    r = subprocess.run(
        [sys.executable, "-S", "-c", _PRELUDE + body],
        env=env,
        capture_output=True,
        timeout=30,
    )
    assert r.returncode == 0, r.stderr.decode()
    return json.loads(r.stdout.decode().strip().splitlines()[-1])


def test_pth_imports_nothing_when_autoload_is_unset():
    out = _run("print(json.dumps(loaded('smeltr')))", {})
    assert out == []


def test_autoload_does_not_import_mlx_into_a_program_that_never_uses_it(fake_daemon):
    # Scopes, marks and the exit path (snapshot + detach) included: the exit
    # snapshot used to import mlx.core to list its streams.
    body = """
import smeltr
from smeltr import _shutdown
with smeltr.scope("work"):
    smeltr.mark("m")
_shutdown._atexit_handler()
print(json.dumps({'mlx': loaded('mlx'), 'safetensors': loaded('safetensors'),
                  'torch': loaded('torch')}))
"""
    out = _run(body, {"SMELTR_AUTOLOAD": "1", "SMELTR_SOCKET": fake_daemon.sock_path})
    assert out == {"mlx": [], "safetensors": [], "torch": []}
    # Still attached: the process announced itself.
    assert fake_daemon.wait_for(
        lambda ms: any(m["payload"]["kind"] == "PythonSidecarHello" for m in ms)
    )


@pytest.mark.parametrize("first_import", ["mlx.core", "mlx.nn"])
def test_autoload_instruments_mlx_once_the_program_imports_it(fake_daemon, first_import):
    pytest.importorskip("mlx.nn")
    body = f"""
import importlib
importlib.import_module({first_import!r})
import mlx.core as mx
import mlx.nn as nn
print(json.dumps({{
    "eval": getattr(mx.eval, "_smeltr_wrapped", False),
    "load": getattr(mx.load, "_smeltr_wrapped", False),
    "linear": getattr(nn.Linear.__call__, "_smeltr_wrapped", False),
    "loader": type(mx.__spec__.loader).__module__.startswith("smeltr"),
}}))
"""
    out = _run(body, {"SMELTR_AUTOLOAD": "1", "SMELTR_SOCKET": fake_daemon.sock_path})
    assert out == {"eval": True, "load": True, "linear": True, "loader": False}


def test_sidecar_hello_reports_the_mlx_version_without_importing_it(fake_daemon):
    pytest.importorskip("mlx.core")
    out = _run(
        "print(json.dumps(loaded('mlx') + loaded('importlib.metadata')))",
        {"SMELTR_AUTOLOAD": "1", "SMELTR_SOCKET": fake_daemon.sock_path},
    )
    assert out == []
    assert fake_daemon.wait_for(
        lambda ms: any(m["payload"]["kind"] == "PythonSidecarHello" for m in ms)
    )
    hello = next(m for m in fake_daemon.emits() if m["payload"]["kind"] == "PythonSidecarHello")
    import importlib.metadata

    assert hello["payload"]["mlx_version"] == importlib.metadata.version("mlx")

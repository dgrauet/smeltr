"""End-to-end: tiny MLX model under smeltr produces a breakdown with Linear nodes."""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import sys
import textwrap
import time
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]

# Sidecar script: define a 3-Linear model, run it, materialise the output.
# smeltr.attach() installs module tracking via _modules.install(), which
# monkey-patches mlx.nn.Module.__call__ so ModuleEntered/ModuleReturned
# events are emitted for each layer invocation.
# decorate_eval() wraps mx.core.eval so MlxEvalEntered/Returned events fire;
# the module_stack captured at that point will be empty because the forward
# pass has already returned — a known Phase-1 limitation.  However,
# ModuleEntered events are still emitted, so `smeltr breakdown` will display
# the module class names in its table.
_SIDECAR_SCRIPT = textwrap.dedent("""\
    import smeltr
    import mlx.core as mx
    import mlx.nn as nn

    smeltr.attach(poll_hz=0)
    smeltr.decorate_eval()

    class M(nn.Module):
        def __init__(self):
            super().__init__()
            self.a = nn.Linear(4, 4)
            self.b = nn.Linear(4, 4)
            self.c = nn.Linear(4, 4)

        def __call__(self, x):
            return self.c(self.b(self.a(x)))

    m = M()
    y = m(mx.zeros((1, 4)))
    mx.eval(y)
    smeltr.detach()
""")


def _cargo_build() -> tuple[Path, Path]:
    subprocess.run(
        ["cargo", "build", "-p", "smeltr-daemon", "-p", "smeltr-cli"],
        cwd=REPO_ROOT,
        check=True,
    )
    target = REPO_ROOT / "target" / "debug"
    return target / "smeltrd", target / "smeltr"


@pytest.mark.timeout(180)
def test_breakdown_e2e_finds_linear_modules(short_tmp_dir, tmp_path):
    """Run a 3-Linear MLX model under smeltr and verify breakdown sees the modules."""
    if not shutil.which("cargo"):
        pytest.skip("cargo not available")
    pytest.importorskip("mlx.core")
    pytest.importorskip("mlx.nn")
    if sys.platform != "darwin":
        pytest.skip("smeltr metal-hook is macOS-only")

    smeltrd, smeltr_cli = _cargo_build()

    smeltr_home = Path(short_tmp_dir) / "home"
    smeltr_home.mkdir()
    sock = Path(short_tmp_dir) / "smeltrd.sock"
    env = {
        **os.environ,
        "SMELTR_HOME": str(smeltr_home),
        "SMELTR_SOCKET": str(sock),
        "RUST_LOG": "warn",
    }

    daemon = subprocess.Popen(
        [str(smeltrd), "--foreground"],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:
        for _ in range(50):
            if sock.exists():
                break
            time.sleep(0.1)
        else:
            daemon.terminate()
            pytest.fail("smeltrd did not create the socket in time")

        # Write the sidecar script to a temp file so we avoid any shell-quoting
        # issues with the multi-line module definition.
        sidecar_py = tmp_path / "sidecar.py"
        sidecar_py.write_text(_SIDECAR_SCRIPT)

        sidecar = subprocess.run(
            [sys.executable, str(sidecar_py)],
            env=env,
            capture_output=True,
            text=True,
            timeout=60,
        )
        assert sidecar.returncode == 0, (
            f"sidecar failed:\nstdout={sidecar.stdout!r}\nstderr={sidecar.stderr!r}"
        )

        # Stop the daemon so the active session is finalized and the event
        # buffer flushed to disk before we read it back via the CLI.
        # Fall back to SIGTERM if the graceful stop fails or times out.
        try:
            stop = subprocess.run(
                [str(smeltr_cli), "daemon", "stop"],
                env=env,
                capture_output=True,
                text=True,
                timeout=15,
            )
            if stop.returncode != 0:
                daemon.send_signal(signal.SIGTERM)
        except subprocess.TimeoutExpired:
            daemon.send_signal(signal.SIGTERM)
        try:
            daemon.wait(timeout=15)
        except subprocess.TimeoutExpired:
            daemon.kill()
            daemon.wait(timeout=5)
            pytest.fail("smeltrd did not exit within 15s after stop")

    finally:
        if daemon.poll() is None:
            daemon.terminate()
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=5)

    breakdown = subprocess.run(
        [str(smeltr_cli), "breakdown", "--last"],
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert breakdown.returncode == 0, (
        f"smeltr breakdown failed:\nstdout={breakdown.stdout!r}\nstderr={breakdown.stderr!r}"
    )
    out = breakdown.stdout
    assert "Linear" in out, f"expected 'Linear' in stdout; got:\n{out}"

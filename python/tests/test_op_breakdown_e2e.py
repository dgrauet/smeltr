"""End-to-end: smeltr record on a tiny MLX model produces op lines in the
breakdown.

What is under test is that op-level capture happened at all, not which
naming path produced the label. The hook builds a synthetic identity from
the MTLComputePipelineState pointer + threadgroup dimensions
(`K_<pso_hash16>_<w>x<h>x<d>`), and since #21 also resolves a symbol,
which the renderer prefers. On a working machine the symbol is the normal
outcome, so asserting on the `K_` fallback asserted the degraded path and
failed *because the tool improved* (#226). The precedence between the two
is covered where it belongs, in
`smeltr_analyzer::breakdown::render_table_prefers_symbol_over_fingerprint`,
which pins both directions — do not re-add a naming assertion here.

Skips when MLX isn't installed, not on macOS, or when op-level capture
is disabled (SMELTR_HOOK_NO_OPS=1). CI installs `.[mlx,dev]` so the MLX
skip does not silently hide this test there.
"""

from __future__ import annotations

import os
import re
import shutil
import signal
import subprocess
import sys
import textwrap
import time
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]

# Tiny model that exercises matmul (Linear) + softmax. Run under smeltr record
# so the metal-hook is injected + Python autoload triggers smeltr.attach.
_MODEL_SCRIPT = textwrap.dedent("""\
    import mlx.core as mx
    import mlx.nn as nn

    class M(nn.Module):
        def __init__(self):
            super().__init__()
            self.fc = nn.Linear(256, 256)

        def __call__(self, x):
            return mx.softmax(self.fc(x))

    m = M()
    x = mx.random.normal((1, 256))
    y = m(x)
    mx.eval(y)
""")


def _cargo_build() -> tuple[Path, Path]:
    subprocess.run(
        ["cargo", "build", "-p", "smeltr-daemon", "-p", "smeltr-cli"],
        cwd=REPO_ROOT,
        check=True,
    )
    # We also need the metal-hook dylib (smeltr record will extract the embedded
    # copy if needed, but a fresh build keeps things current).
    subprocess.run(
        ["make", "-C", str(REPO_ROOT / "metal-hook"), "clean", "all"],
        check=True,
    )
    target = REPO_ROOT / "target" / "debug"
    return target / "smeltrd", target / "smeltr"


@pytest.mark.timeout(240)
def test_op_breakdown_records_some_op(short_tmp_dir, tmp_path):
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

        model_py = tmp_path / "model.py"
        model_py.write_text(_MODEL_SCRIPT)

        # `smeltr record` injects the metal-hook dylib + sets SMELTR_AUTOLOAD=1
        # so the python sidecar attaches automatically. Phase 2.5 op-level
        # capture activates inside the dylib via PSO pointer + threadgroup dims.
        record = subprocess.run(
            [str(smeltr_cli), "record", sys.executable, str(model_py)],
            env=env,
            capture_output=True,
            text=True,
            timeout=120,
        )
        assert record.returncode == 0, (
            f"smeltr record failed:\nstdout={record.stdout!r}\nstderr={record.stderr!r}"
        )

        # Stop the daemon so the session is finalized.
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
        [str(smeltr_cli), "breakdown", "--last", "--top-ops", "10"],
        env=env,
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert breakdown.returncode == 0, (
        f"smeltr breakdown failed:\nstdout={breakdown.stdout!r}\nstderr={breakdown.stderr!r}"
    )
    out = breakdown.stdout

    # The hook emits "op-level capture disabled" to its own stderr (which is
    # part of `record`'s stderr), NOT to `smeltr breakdown` stdout.  Check
    # the record run's stderr for the skip condition.
    if "op-level capture disabled" in record.stderr:
        pytest.skip("op-level capture disabled — SMELTR_HOOK_NO_OPS=1 or hook inactive")

    # Op-level capture produced at least one named kernel. Either naming path
    # is a pass: a resolved symbol ("block_softmax_float32") or the synthetic
    # fallback ("K_a3f7_32x1x1"). What must not happen is an empty label or no
    # op line at all.
    op_names = re.findall(r"\u2514 op:(\S+)", out)
    assert op_names, f"expected at least one op line in breakdown:\n{out}"

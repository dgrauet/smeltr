"""End-to-end: real smeltrd + real CLI + python sidecar.

Compiles the workspace binaries (cargo) and exercises the wire protocol
against the production server, not the fake one from conftest.py.
"""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]


def _cargo_build() -> tuple[Path, Path]:
    subprocess.run(
        ["cargo", "build", "-p", "smeltr-daemon", "-p", "smeltr-cli"],
        cwd=REPO_ROOT, check=True,
    )
    target = REPO_ROOT / "target" / "debug"
    return target / "smeltrd", target / "smeltr"


@pytest.mark.timeout(120)
def test_python_sidecar_emits_to_real_daemon(short_tmp_dir):
    if not shutil.which("cargo"):
        pytest.skip("cargo not available")
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
        env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    try:
        for _ in range(50):
            if sock.exists():
                break
            time.sleep(0.1)
        else:
            daemon.terminate()
            pytest.fail("smeltrd did not create the socket in time")

        script = (
            "import smeltr;"
            "smeltr.attach(poll_hz=0);"
            "smeltr.mark('hello-from-python');"
            "smeltr.detach()"
        )
        result = subprocess.run(
            [sys.executable, "-c", script],
            env=env, capture_output=True, text=True, timeout=10,
        )
        assert result.returncode == 0, (
            f"sidecar failed: stdout={result.stdout!r} stderr={result.stderr!r}"
        )

        # Stop the daemon so the active session is finalized and the event
        # buffer flushed to disk before we read it back via the CLI.
        # Plan 5 Task 1 bumped `daemon stop` timeout to 10s, so it can
        # complete full session finalisation. Fall back to SIGTERM if it
        # fails.
        try:
            stop = subprocess.run(
                [str(smeltr_cli), "daemon", "stop"],
                env=env, capture_output=True, text=True, timeout=15,
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

        sessions = subprocess.run(
            [str(smeltr_cli), "sessions", "ls"],
            env=env, capture_output=True, text=True, check=True,
        )
        assert sessions.stdout.strip(), f"no sessions: {sessions.stdout!r}"
        first_line = sessions.stdout.strip().splitlines()[0]
        sid = first_line.split()[0]

        show = subprocess.run(
            [str(smeltr_cli), "sessions", "show", sid],
            env=env, capture_output=True, text=True, check=True,
        )
        text = show.stdout
        assert "PythonSidecarHello" in text, f"missing PythonSidecarHello in:\n{text}"
        assert "hello-from-python" in text, f"missing mark label in:\n{text}"
    finally:
        if daemon.poll() is None:
            daemon.terminate()
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.wait(timeout=5)

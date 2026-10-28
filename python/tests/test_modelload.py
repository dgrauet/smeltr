"""Tests for smeltr._modelload — safetensors / mlx.core.load wrapper."""

from __future__ import annotations

import gc
import hashlib
import os
import sys
import types
from typing import Any

import pytest

import smeltr
from smeltr import _modelload


def _count(msgs, kind: str) -> int:
    return sum(1 for m in msgs if m["payload"]["kind"] == kind)


@pytest.fixture(autouse=True)
def _reset_modelload():
    _modelload._undecorate_for_tests()
    yield
    _modelload._undecorate_for_tests()


# ---------------------------------------------------------------------------
# Idempotency
# ---------------------------------------------------------------------------


def test_decorate_model_loads_idempotent(monkeypatch, fake_daemon):
    """Calling decorate_model_loads() twice must not double-wrap."""
    smeltr.attach(poll_hz=0)
    try:
        # Create a minimal safetensors stub in sys.modules
        st_mod = types.ModuleType("safetensors")
        calls: list[str] = []

        def _fake_safe_open(filename: str, *args: Any, **kwargs: Any) -> dict:
            calls.append(filename)
            return {}

        st_mod.safe_open = _fake_safe_open  # type: ignore[attr-defined]
        monkeypatch.setitem(sys.modules, "safetensors", st_mod)

        _modelload.decorate_model_loads()
        _modelload.decorate_model_loads()  # second call — must be a no-op

        assert getattr(st_mod.safe_open, "_smeltr_wrapped", False), "safe_open should be wrapped"
        # The original is reachable exactly once
        original = st_mod.safe_open._smeltr_original
        assert not getattr(original, "_smeltr_wrapped", False), "must not double-wrap"
    finally:
        smeltr.detach()


# ---------------------------------------------------------------------------
# ModelLoad events emitted for safetensors.safe_open
# ---------------------------------------------------------------------------


def test_safetensors_safe_open_emits_model_load(monkeypatch, fake_daemon, tmp_path):
    """A ModelLoad event is emitted when safetensors.safe_open is called."""
    # Write a real file so os.path.getsize works.
    model_file = tmp_path / "model.safetensors"
    model_file.write_bytes(b"x" * 128)
    canonical = os.path.realpath(str(model_file))

    # Stub safetensors module
    st_mod = types.ModuleType("safetensors")

    def _fake_safe_open(filename: str, *args: Any, **kwargs: Any) -> dict:
        return {}

    st_mod.safe_open = _fake_safe_open  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "safetensors", st_mod)
    # Remove safetensors.torch so it doesn't interfere
    monkeypatch.delitem(sys.modules, "safetensors.torch", raising=False)

    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        st_mod.safe_open(str(model_file), framework="pt")
    finally:
        smeltr.detach()

    loads = [m for m in fake_daemon.received if m["payload"]["kind"] == "ModelLoad"]
    assert len(loads) == 1
    p = loads[0]["payload"]
    assert p["path"] == canonical
    assert p["size_bytes"] == 128
    assert p["framework"] == "safetensors"
    assert p["sha8"] == hashlib.sha256(canonical.encode()).hexdigest()[:8]
    assert p["t_end_ns"] >= p["t_start_ns"]


# ---------------------------------------------------------------------------
# ModelLoad events emitted for safetensors.torch.load_file
# ---------------------------------------------------------------------------


def test_safetensors_torch_load_file_emits_model_load(monkeypatch, fake_daemon, tmp_path):
    """A ModelLoad event is emitted when safetensors.torch.load_file is called."""
    model_file = tmp_path / "model2.safetensors"
    model_file.write_bytes(b"y" * 256)
    canonical = os.path.realpath(str(model_file))

    st_torch_mod = types.ModuleType("safetensors.torch")

    def _fake_load_file(filename: str, *args: Any, **kwargs: Any) -> dict:
        return {}

    st_torch_mod.load_file = _fake_load_file  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "safetensors.torch", st_torch_mod)

    # Also provide a parent safetensors module so the import chain works
    if "safetensors" not in sys.modules:
        monkeypatch.setitem(sys.modules, "safetensors", types.ModuleType("safetensors"))

    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        st_torch_mod.load_file(str(model_file))
    finally:
        smeltr.detach()

    loads = [m for m in fake_daemon.received if m["payload"]["kind"] == "ModelLoad"]
    assert len(loads) == 1
    p = loads[0]["payload"]
    assert p["path"] == canonical
    assert p["size_bytes"] == 256
    assert p["framework"] == "safetensors"


# ---------------------------------------------------------------------------
# ModelLoad events emitted for mlx.core.load
# ---------------------------------------------------------------------------


def test_mlx_core_load_emits_model_load(monkeypatch, fake_daemon, tmp_path):
    """A ModelLoad event is emitted when mlx.core.load is called."""
    model_file = tmp_path / "model.npz"
    model_file.write_bytes(b"z" * 512)
    canonical = os.path.realpath(str(model_file))

    mx_core_mod = types.ModuleType("mlx.core")

    def _fake_load(file: str, *args: Any, **kwargs: Any) -> dict:
        return {}

    mx_core_mod.load = _fake_load  # type: ignore[attr-defined]

    mlx_mod = types.ModuleType("mlx")
    monkeypatch.setitem(sys.modules, "mlx", mlx_mod)
    monkeypatch.setitem(sys.modules, "mlx.core", mx_core_mod)
    mlx_mod.core = mx_core_mod  # type: ignore[attr-defined]

    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        mx_core_mod.load(str(model_file))
    finally:
        smeltr.detach()

    loads = [m for m in fake_daemon.received if m["payload"]["kind"] == "ModelLoad"]
    assert len(loads) == 1
    p = loads[0]["payload"]
    assert p["path"] == canonical
    assert p["size_bytes"] == 512
    assert p["framework"] == "mlx"


# ---------------------------------------------------------------------------
# Non-string path arg — emission silently skipped
# ---------------------------------------------------------------------------


def test_non_string_path_skips_emission_silently(monkeypatch, fake_daemon):
    """If path arg is not a string/PathLike, no ModelLoad event is emitted."""
    st_mod = types.ModuleType("safetensors")

    def _fake_safe_open(filename: Any, *args: Any, **kwargs: Any) -> dict:
        return {}

    st_mod.safe_open = _fake_safe_open  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "safetensors", st_mod)
    monkeypatch.delitem(sys.modules, "safetensors.torch", raising=False)

    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        # Pass a BytesIO-like object (an int here suffices as a non-PathLike)
        st_mod.safe_open(12345)
    finally:
        smeltr.detach()

    loads = [m for m in fake_daemon.received if m["payload"]["kind"] == "ModelLoad"]
    assert loads == []


# ---------------------------------------------------------------------------
# Missing file — os.path.getsize fails — emission silently skipped
# ---------------------------------------------------------------------------


def test_missing_file_skips_emission_silently(monkeypatch, fake_daemon):
    """If getsize raises OSError, no ModelLoad event is emitted."""
    st_mod = types.ModuleType("safetensors")

    def _fake_safe_open(filename: Any, *args: Any, **kwargs: Any) -> dict:
        return {}

    st_mod.safe_open = _fake_safe_open  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "safetensors", st_mod)
    monkeypatch.delitem(sys.modules, "safetensors.torch", raising=False)

    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        # Path does not exist on disk
        st_mod.safe_open("/nonexistent/path/model.safetensors")
    finally:
        smeltr.detach()

    loads = [m for m in fake_daemon.received if m["payload"]["kind"] == "ModelLoad"]
    assert loads == []


# ---------------------------------------------------------------------------
# ModelUnload events via weakref.finalize
# ---------------------------------------------------------------------------


def test_mx_load_emits_unload_when_result_dropped(monkeypatch, fake_daemon, tmp_path):
    """Dropping the mx.load result dict triggers a ModelUnload event."""
    model_file = tmp_path / "weights.npz"
    model_file.write_bytes(b"z" * 256)
    canonical = os.path.realpath(str(model_file))
    expected_sha8 = hashlib.sha256(canonical.encode()).hexdigest()[:8]

    mx_core_mod = types.ModuleType("mlx.core")

    def _fake_load(file: str, *args: Any, **kwargs: Any) -> dict:
        # Return a plain dict; the wrapper will convert it to _WeakRefableDict.
        return {"w": object()}

    mx_core_mod.load = _fake_load  # type: ignore[attr-defined]

    mlx_mod = types.ModuleType("mlx")
    monkeypatch.setitem(sys.modules, "mlx", mlx_mod)
    monkeypatch.setitem(sys.modules, "mlx.core", mx_core_mod)
    mlx_mod.core = mx_core_mod  # type: ignore[attr-defined]

    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        result = mx_core_mod.load(str(model_file))
        # Verify ModelLoad was emitted.
        assert fake_daemon.wait_for(lambda ms: _count(ms, "ModelLoad") >= 1)
        loads = [m for m in fake_daemon.received if m["payload"]["kind"] == "ModelLoad"]
        assert len(loads) == 1

        # Drop the reference; force GC so the finalizer fires.
        del result
        gc.collect()

        assert fake_daemon.wait_for(lambda ms: _count(ms, "ModelUnload") >= 1)
        unloads = [m for m in fake_daemon.received if m["payload"]["kind"] == "ModelUnload"]
        assert len(unloads) == 1, f"expected 1 ModelUnload, got {len(unloads)}"
        p = unloads[0]["payload"]
        assert p["path"] == canonical
        assert p["sha8"] == expected_sha8
        assert isinstance(p["t_ns"], int)
        assert p["t_ns"] > 0
    finally:
        smeltr.detach()


def test_safetensors_safe_open_emits_unload_after_exit(monkeypatch, fake_daemon, tmp_path):
    """After the safe_open object is GC'd a ModelUnload event is emitted."""
    model_file = tmp_path / "model2.safetensors"
    model_file.write_bytes(b"x" * 512)
    canonical = os.path.realpath(str(model_file))
    expected_sha8 = hashlib.sha256(canonical.encode()).hexdigest()[:8]

    st_mod = types.ModuleType("safetensors")

    # Return a plain object to simulate a safe_open handle.
    class FakeSafeOpen:
        pass

    def _fake_safe_open(filename: str, *args: Any, **kwargs: Any) -> FakeSafeOpen:
        return FakeSafeOpen()

    st_mod.safe_open = _fake_safe_open  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "safetensors", st_mod)
    monkeypatch.delitem(sys.modules, "safetensors.torch", raising=False)

    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        result = st_mod.safe_open(str(model_file), framework="pt")
        assert fake_daemon.wait_for(lambda ms: _count(ms, "ModelLoad") >= 1)
        loads = [m for m in fake_daemon.received if m["payload"]["kind"] == "ModelLoad"]
        assert len(loads) == 1

        del result
        gc.collect()

        assert fake_daemon.wait_for(lambda ms: _count(ms, "ModelUnload") >= 1)
        unloads = [m for m in fake_daemon.received if m["payload"]["kind"] == "ModelUnload"]
        assert len(unloads) == 1, f"expected 1 ModelUnload, got {len(unloads)}"
        p = unloads[0]["payload"]
        assert p["path"] == canonical
        assert p["sha8"] == expected_sha8
    finally:
        smeltr.detach()


# ---------------------------------------------------------------------------
# Tensors loaded but never read (#308)
# ---------------------------------------------------------------------------


@pytest.fixture
def fake_mx_load(monkeypatch, tmp_path):
    """A stub mlx.core.load returning `weights` (a plain dict), plus the file path."""
    model_file = tmp_path / "lora.safetensors"
    model_file.write_bytes(b"l" * 128)
    state: dict[str, Any] = {"weights": {}}

    mx_core_mod = types.ModuleType("mlx.core")
    mx_core_mod.load = lambda file, *a, **k: dict(state["weights"])  # type: ignore[attr-defined]
    mlx_mod = types.ModuleType("mlx")
    mlx_mod.core = mx_core_mod  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "mlx", mlx_mod)
    monkeypatch.setitem(sys.modules, "mlx.core", mx_core_mod)

    def load(weights: dict[str, Any]) -> Any:
        state["weights"] = weights
        return mx_core_mod.load(str(model_file))

    return load


def _unload_after(fake_daemon, drop) -> dict:
    """Run `drop` (which must release the loaded dict), return the ModelUnload payload."""
    drop()
    gc.collect()
    assert fake_daemon.wait_for(lambda ms: _count(ms, "ModelUnload") >= 1)
    unloads = [m for m in fake_daemon.received if m["payload"]["kind"] == "ModelUnload"]
    assert len(unloads) == 1
    return unloads[0]["payload"]


LORA = {
    "blocks.0.attn.to_q.lora_A.weight": 1,
    "blocks.0.attn.to_q.lora_B.weight": 2,
    "proj_in.lora_A.weight": 3,
    "proj_in.lora_B.weight": 4,
}


def test_unload_lists_keys_whose_value_was_never_read(fake_daemon, fake_mx_load):
    """ltx-2-mlx #235: every key iterated, only the block keys' values read."""
    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        holder = {"d": fake_mx_load(LORA)}
        for key in holder["d"]:
            if key.startswith("blocks."):
                _ = holder["d"][key]
        p = _unload_after(fake_daemon, holder.clear)
        assert p["tensor_count"] == 4
        assert p["unread_count"] == 2
        assert sorted(p["unread_keys"]) == ["proj_in.lora_A.weight", "proj_in.lora_B.weight"]
    finally:
        smeltr.detach()


def test_keys_len_and_membership_are_not_reads(fake_daemon, fake_mx_load):
    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        holder = {"d": fake_mx_load(LORA)}
        d = holder["d"]
        _ = len(d), list(d.keys()), "proj_in.lora_A.weight" in d, sorted(d)
        del d
        p = _unload_after(fake_daemon, holder.clear)
        assert p["unread_count"] == 4
    finally:
        smeltr.detach()


def test_get_and_pop_are_reads(fake_daemon, fake_mx_load):
    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        holder = {"d": fake_mx_load(LORA)}
        d = holder["d"]
        d.get("blocks.0.attn.to_q.lora_A.weight")
        d.pop("blocks.0.attn.to_q.lora_B.weight")
        d.setdefault("proj_in.lora_A.weight")
        del d
        p = _unload_after(fake_daemon, holder.clear)
        assert p["unread_keys"] == ["proj_in.lora_B.weight"]
    finally:
        smeltr.detach()


@pytest.mark.parametrize(
    "consume",
    [
        lambda d: list(d.items()),
        lambda d: list(d.values()),
        lambda d: d.copy(),
        lambda d: dict(d),
        lambda d: {**d},
        lambda d: {}.update(d),
        lambda d: d.popitem() and list(d.values()),
    ],
    ids=["items", "values", "copy", "dict", "unpack", "update", "popitem"],
)
def test_bulk_access_counts_every_value_as_read(fake_daemon, fake_mx_load, consume):
    """When the value may have escaped, report nothing rather than a false positive."""
    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        holder = {"d": fake_mx_load(LORA)}
        consume(holder["d"])
        p = _unload_after(fake_daemon, holder.clear)
        assert p["tensor_count"] == 4
        assert p["unread_count"] == 0
        assert p.get("unread_keys", []) == []
    finally:
        smeltr.detach()


def test_unread_keys_are_capped_but_counted(fake_daemon, fake_mx_load):
    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        many = {f"layer.{i}.weight": i for i in range(1000)}
        holder = {"d": fake_mx_load(many)}
        p = _unload_after(fake_daemon, holder.clear)
        assert p["tensor_count"] == 1000
        assert p["unread_count"] == 1000
        assert len(p["unread_keys"]) == _modelload.UNREAD_KEYS_CAP
    finally:
        smeltr.detach()


def test_loaded_dict_still_behaves_like_the_original(fake_mx_load, fake_daemon):
    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        d = fake_mx_load(LORA)
        assert isinstance(d, dict)
        assert d == LORA
        assert list(d) == list(LORA)
        assert d["proj_in.lora_A.weight"] == 3
        assert d.get("missing", "x") == "x"
        with pytest.raises(KeyError):
            d["missing"]
        d["new"] = 5
        assert d.pop("new") == 5
    finally:
        smeltr.detach()


def test_mx_eval_of_the_loaded_dict_counts_as_reading_it():
    """mx.eval walks a dict in C (PyDict_Next), past the read-tracking accessors."""
    mx = pytest.importorskip("mlx.core")
    from smeltr import _mlx

    _mlx.decorate_eval()
    try:
        direct = _modelload._WeakRefableDict({"w": mx.zeros((2,)), "b": mx.zeros((2,))})
        mx.eval(direct)
        assert direct._smeltr_reads.unread() == []

        nested = _modelload._WeakRefableDict({"w": mx.zeros((2,))})
        mx.eval([nested])
        assert nested._smeltr_reads.unread() == []

        untouched = _modelload._WeakRefableDict({"w": mx.zeros((2,))})
        mx.eval(mx.zeros((1,)))
        assert untouched._smeltr_reads.unread() == ["w"]
    finally:
        _mlx._undecorate_eval_for_tests()


def test_unload_names_the_path_the_program_passed(monkeypatch, fake_daemon, tmp_path):
    """HF cache paths are symlinks to hash-named blobs: keep the readable name."""
    blob = tmp_path / "blobs" / "2943ab994f3c9d88"
    blob.parent.mkdir()
    blob.write_bytes(b"b" * 64)
    link = tmp_path / "distilled-lora.safetensors"
    link.symlink_to(blob)

    mx_core_mod = types.ModuleType("mlx.core")
    mx_core_mod.load = lambda file, *a, **k: {"w": 1}  # type: ignore[attr-defined]
    mlx_mod = types.ModuleType("mlx")
    mlx_mod.core = mx_core_mod  # type: ignore[attr-defined]
    monkeypatch.setitem(sys.modules, "mlx", mlx_mod)
    monkeypatch.setitem(sys.modules, "mlx.core", mx_core_mod)

    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        holder = {"d": mx_core_mod.load(str(link))}
        p = _unload_after(fake_daemon, holder.clear)
        assert p["path"] == os.path.realpath(str(blob))
        assert p["requested_path"] == str(link)
    finally:
        smeltr.detach()


def test_unload_omits_requested_path_when_it_is_already_canonical(fake_daemon, fake_mx_load):
    smeltr.attach(poll_hz=0)
    try:
        _modelload.decorate_model_loads()
        holder = {"d": fake_mx_load(LORA)}
        p = _unload_after(fake_daemon, holder.clear)
        assert "requested_path" not in p
    finally:
        smeltr.detach()

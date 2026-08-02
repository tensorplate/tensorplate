"""Tests for the MPS-backed package validation fixture."""

from __future__ import annotations

import builtins
import json
import socket
import sys
from pathlib import Path
from types import ModuleType, SimpleNamespace
from typing import Any

import pytest

from tensorplate_pytorch_backend import codec, protocol
from tensorplate_pytorch_backend.backends.base import BackendError
from tensorplate_pytorch_backend.backends.mps_fixture import MpsFixtureBackend
from tensorplate_pytorch_backend.runner import SidecarRunner


class _FakeMps:
    def __init__(self, *, available: bool, calls: dict[str, int]) -> None:
        self._available = available
        self._calls = calls

    def is_built(self) -> bool:
        return True

    def is_available(self) -> bool:
        return self._available

    def synchronize(self) -> None:
        self._calls["synchronize"] += 1


class _FakeTensor:
    def __init__(self, calls: dict[str, int]) -> None:
        self._calls = calls

    def __add__(self, other: object) -> _FakeTensor:
        _ = other
        self._calls["add"] += 1
        return self


def _module(name: str, **attributes: Any) -> ModuleType:
    module = ModuleType(name)
    for key, value in attributes.items():
        setattr(module, key, value)
    return module


def _install_fake_torch(monkeypatch: pytest.MonkeyPatch, *, available: bool) -> dict[str, int]:
    calls = {"ones": 0, "add": 0, "synchronize": 0}

    def ones(shape: tuple[int, ...], *, device: str) -> _FakeTensor:
        assert shape == (1,)
        assert device == "mps"
        calls["ones"] += 1
        return _FakeTensor(calls)

    mps = _FakeMps(available=available, calls=calls)
    torch = _module(
        "torch",
        __version__="2.13.0",
        backends=SimpleNamespace(mps=mps),
        mps=mps,
        ones=ones,
    )
    monkeypatch.setitem(sys.modules, "torch", torch)
    return calls


def _model_spec(config_path: Path) -> dict[str, Any]:
    return {
        "schema_version": "0.1",
        "model_id": "mps-smoke",
        "model_class": "custom",
        "artifact_path": str(config_path),
        "backend_hint": "python_pytorch",
        "precision_hint": "fp32",
    }


def test_default_runner_selects_mps_fixture_from_artifact(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    calls = _install_fake_torch(monkeypatch, available=True)
    config = tmp_path / "mps-smoke.json"
    config.write_text(
        json.dumps({"backend_profile": "mps_fixture", "device": "mps"}),
        encoding="utf-8",
    )

    client, server = socket.socketpair()
    try:
        runner = SidecarRunner(server)
        response = runner._dispatch(
            codec.SidecarFrame(
                header={
                    "schema_version": protocol.SCHEMA_VERSION,
                    "message_id": "load-mps-smoke",
                    "kind": protocol.KIND_LOAD_MODEL,
                    "model_spec": _model_spec(config),
                }
            )
        )
        assert response is not None
        assert response.header["status"] == protocol.STATUS_OK
        assert response.header["runtime_capability"]["accelerator_runtime_available"] is True
        assert runner.state.backend_factory_name == "mps_fixture"
        assert calls == {"ones": 1, "add": 1, "synchronize": 1}
    finally:
        client.close()
        server.close()


def test_mps_fixture_rejects_unavailable_runtime_before_tensor_operation(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    calls = _install_fake_torch(monkeypatch, available=False)
    config = tmp_path / "mps-smoke.json"
    config.write_text(
        json.dumps({"backend_profile": "mps_fixture", "device": "mps"}),
        encoding="utf-8",
    )

    backend = MpsFixtureBackend()
    with pytest.raises(BackendError) as caught:
        backend.load(_model_spec(config))
    assert caught.value.code == protocol.ERR_UNSUPPORTED
    assert calls == {"ones": 0, "add": 0, "synchronize": 0}


def test_mps_fixture_maps_broken_torch_runtime_to_load_failure(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    config = tmp_path / "mps-smoke.json"
    config.write_text(
        json.dumps({"backend_profile": "mps_fixture", "device": "mps"}),
        encoding="utf-8",
    )
    real_import = builtins.__import__

    def broken_import(
        name: str,
        globals: dict[str, Any] | None = None,
        locals: dict[str, Any] | None = None,
        fromlist: tuple[str, ...] = (),
        level: int = 0,
    ) -> Any:
        if name == "torch":
            raise OSError("broken PyTorch dynamic library")
        return real_import(name, globals, locals, fromlist, level)

    monkeypatch.setattr(builtins, "__import__", broken_import)
    backend = MpsFixtureBackend()
    with pytest.raises(BackendError) as caught:
        backend.load(_model_spec(config))
    assert caught.value.code == protocol.ERR_LOAD_FAILED

"""Tests for the CUDA-backed package validation fixture."""

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
from tensorplate_pytorch_backend.accelerator import probe_cuda_runtime
from tensorplate_pytorch_backend.backends.base import BackendError
from tensorplate_pytorch_backend.backends.cuda_fixture import CudaFixtureBackend
from tensorplate_pytorch_backend.runner import SidecarRunner

_EXTENT = 64


class _FakeScalar:
    def __init__(self, value: float) -> None:
        self._value = value

    def item(self) -> float:
        return self._value


class _FakeTensor:
    def __init__(self, calls: dict[str, int], *, product_sum: float) -> None:
        self._calls = calls
        self._product_sum = product_sum

    def __matmul__(self, other: object) -> _FakeTensor:
        _ = other
        self._calls["matmul"] += 1
        return self

    def sum(self) -> _FakeScalar:
        self._calls["sum"] += 1
        return _FakeScalar(self._product_sum)


def _module(name: str, **attributes: Any) -> ModuleType:
    module = ModuleType(name)
    for key, value in attributes.items():
        setattr(module, key, value)
    return module


def _install_fake_torch(
    monkeypatch: pytest.MonkeyPatch,
    *,
    available: bool,
    built_version: str | None = "12.9",
    product_sum: float = float(_EXTENT**3),
) -> dict[str, int]:
    calls = {"ones": 0, "matmul": 0, "sum": 0, "synchronize": 0}

    def ones(shape: tuple[int, ...], *, device: str) -> _FakeTensor:
        assert shape == (_EXTENT, _EXTENT)
        assert device == "cuda"
        calls["ones"] += 1
        return _FakeTensor(calls, product_sum=product_sum)

    def synchronize() -> None:
        calls["synchronize"] += 1

    torch = _module(
        "torch",
        __version__="2.13.0",
        version=SimpleNamespace(cuda=built_version),
        cuda=SimpleNamespace(is_available=lambda: available, synchronize=synchronize),
        ones=ones,
    )
    monkeypatch.setitem(sys.modules, "torch", torch)
    return calls


def _model_spec(config_path: Path) -> dict[str, Any]:
    return {
        "schema_version": "0.1",
        "model_id": "cuda-smoke",
        "model_class": "custom",
        "artifact_path": str(config_path),
        "backend_hint": "python_pytorch",
        "precision_hint": "fp32",
    }


def _config(tmp_path: Path, *, device: str = "cuda") -> Path:
    config = tmp_path / "cuda-smoke.json"
    config.write_text(
        json.dumps({"backend_profile": "cuda_fixture", "device": device}),
        encoding="utf-8",
    )
    return config


def test_probe_reports_the_toolkit_version_the_framework_was_built_against() -> None:
    torch = SimpleNamespace(
        __version__="2.13.0",
        version=SimpleNamespace(cuda="12.9"),
        cuda=SimpleNamespace(is_available=lambda: True),
    )
    assert probe_cuda_runtime(torch).to_wire() == {
        "backend_name": "python_pytorch",
        "framework_version": "2.13.0",
        "accelerator_runtime_version": "12.9",
        "accelerator_runtime_built": True,
        "accelerator_runtime_available": True,
    }


def test_probe_fails_closed_on_a_cpu_only_framework_build() -> None:
    torch = SimpleNamespace(
        __version__="2.13.0",
        version=SimpleNamespace(cuda=None),
        cuda=SimpleNamespace(is_available=lambda: True),
    )
    capability = probe_cuda_runtime(torch)
    assert capability.accelerator_runtime_built is False
    assert capability.accelerator_runtime_available is False
    assert capability.unavailable_reason == protocol.REASON_ACCELERATOR_RUNTIME_UNAVAILABLE


def test_default_runner_selects_cuda_fixture_from_artifact(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    calls = _install_fake_torch(monkeypatch, available=True)
    client, server = socket.socketpair()
    try:
        runner = SidecarRunner(server)
        response = runner._dispatch(
            codec.SidecarFrame(
                header={
                    "schema_version": protocol.SCHEMA_VERSION,
                    "message_id": "load-cuda-smoke",
                    "kind": protocol.KIND_LOAD_MODEL,
                    "model_spec": _model_spec(_config(tmp_path)),
                }
            )
        )
        assert response is not None
        assert response.header["status"] == protocol.STATUS_OK
        capability = response.header["runtime_capability"]
        assert capability["accelerator_runtime_available"] is True
        assert capability["accelerator_runtime_version"] == "12.9"
        assert runner.state.backend_factory_name == "cuda_fixture"
        assert calls == {"ones": 1, "matmul": 1, "sum": 1, "synchronize": 1}
    finally:
        client.close()
        server.close()


def test_cuda_fixture_rejects_unavailable_runtime_before_tensor_operation(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    calls = _install_fake_torch(monkeypatch, available=False)
    backend = CudaFixtureBackend()
    with pytest.raises(BackendError) as caught:
        backend.load(_model_spec(_config(tmp_path)))
    assert caught.value.code == protocol.ERR_UNSUPPORTED
    assert calls == {"ones": 0, "matmul": 0, "sum": 0, "synchronize": 0}


def test_cuda_fixture_rejects_a_config_that_does_not_request_cuda(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    calls = _install_fake_torch(monkeypatch, available=True)
    backend = CudaFixtureBackend()
    with pytest.raises(BackendError) as caught:
        backend.load(_model_spec(_config(tmp_path, device="cpu")))
    assert caught.value.code == protocol.ERR_CONFIG_INVALID
    assert calls == {"ones": 0, "matmul": 0, "sum": 0, "synchronize": 0}


def test_cuda_fixture_refuses_a_kernel_that_computed_the_wrong_answer(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    _install_fake_torch(monkeypatch, available=True, product_sum=float(_EXTENT**3) - 1.0)
    backend = CudaFixtureBackend()
    with pytest.raises(BackendError) as caught:
        backend.load(_model_spec(_config(tmp_path)))
    assert caught.value.code == protocol.ERR_LOAD_FAILED
    assert "expected" in caught.value.code_message


def test_cuda_fixture_maps_broken_torch_runtime_to_load_failure(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    config = _config(tmp_path)
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
    backend = CudaFixtureBackend()
    with pytest.raises(BackendError) as caught:
        backend.load(_model_spec(config))
    assert caught.value.code == protocol.ERR_LOAD_FAILED

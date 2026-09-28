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

from tensorplate_pytorch_backend import accelerator, codec, protocol
from tensorplate_pytorch_backend.accelerator import probe_cuda_runtime, require_cuda_runtime
from tensorplate_pytorch_backend.backends.base import BackendError
from tensorplate_pytorch_backend.backends.cuda_fixture import CudaFixtureBackend
from tensorplate_pytorch_backend.runner import SidecarRunner

_EXTENT = 64

# /proc/driver/nvidia/version as a 595.84 open kernel module wrote it, with the
# driver's build host replaced by the synthetic identity.
_DRIVER_REPORT = (
    "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  595.84  Release Build  "
    "(tp-synthetic-operator@tp-synthetic-host)  Wed Jun 10 21:06:37 UTC 2026\n"
    "GCC version:  gcc version 13.3.0 (Ubuntu 13.3.0-6ubuntu2~24.04.1) \n"
)


@pytest.fixture(autouse=True)
def driver_report(monkeypatch: pytest.MonkeyPatch, tmp_path: Path) -> Path:
    """Where the probe reads the driver's report; absent unless a test writes it."""
    path = tmp_path / "nvidia-version"
    monkeypatch.setattr(accelerator, "NVIDIA_DRIVER_VERSION_FILE", path)
    return path


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
    kernel_error: Exception | None = None,
) -> dict[str, int]:
    calls = {"ones": 0, "matmul": 0, "sum": 0, "synchronize": 0}

    def ones(shape: tuple[int, ...], *, device: str) -> _FakeTensor:
        assert shape == (_EXTENT, _EXTENT)
        assert device == "cuda"
        calls["ones"] += 1
        if kernel_error is not None:
            raise kernel_error
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


def _available_torch() -> SimpleNamespace:
    return SimpleNamespace(
        __version__="2.13.0+cu129",
        version=SimpleNamespace(cuda="12.9"),
        cuda=SimpleNamespace(is_available=lambda: True),
    )


def test_probe_reports_the_driver_version_as_the_accelerator_runtime(
    driver_report: Path,
) -> None:
    driver_report.write_text(_DRIVER_REPORT, encoding="utf-8")
    assert probe_cuda_runtime(_available_torch()).to_wire() == {
        "backend_name": "python_pytorch",
        "framework_version": "2.13.0+cu129",
        "accelerator_runtime_version": "595.84",
        "accelerator_runtime_built": True,
        "accelerator_runtime_available": True,
    }


@pytest.mark.parametrize(
    ("report", "version"),
    [
        (_DRIVER_REPORT, "595.84"),
        # The proprietary module's line as the installer tests transcribe it.
        ("NVRM version: NVIDIA UNIX x86_64 Kernel Module  560.35.03\n", "560.35.03"),
    ],
    ids=["open-module-recorded", "proprietary-module-transcribed"],
)
def test_probe_reads_the_version_from_the_kernel_module_line(
    driver_report: Path, report: str, version: str
) -> None:
    driver_report.write_text(report, encoding="utf-8")
    assert probe_cuda_runtime(_available_torch()).accelerator_runtime_version == version


def test_probe_reports_an_unknown_runtime_version_without_a_driver_report() -> None:
    capability = probe_cuda_runtime(_available_torch())
    assert capability.accelerator_runtime_version == "unknown"
    assert capability.accelerator_runtime_built is True


@pytest.mark.parametrize(
    "report",
    [
        "",
        "GCC version:  gcc version 13.3.0 (Ubuntu 13.3.0-6ubuntu2~24.04.1) \n",
        "NVRM version: NVIDIA UNIX Open Kernel Module for x86_64  Release Build\n",
        "NVRM version: NVIDIA UNIX x86_64  595.84  Release Build\n",
        "NVRM version: Kernel Module  595.84  Release Build\n",
    ],
    ids=["empty", "no-nvrm-line", "no-version", "no-module-name", "no-nvidia-unix"],
)
def test_probe_reports_an_unknown_runtime_version_for_an_unrecognized_report(
    driver_report: Path, report: str
) -> None:
    driver_report.write_text(report, encoding="utf-8")
    assert probe_cuda_runtime(_available_torch()).accelerator_runtime_version == "unknown"


def test_an_explicit_runtime_version_overrides_the_driver_report(driver_report: Path) -> None:
    driver_report.write_text(_DRIVER_REPORT, encoding="utf-8")
    capability = probe_cuda_runtime(_available_torch(), accelerator_runtime_version="580.173.02")
    assert capability.accelerator_runtime_version == "580.173.02"


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


class _UnreadableBuildMetadata:
    def __getattr__(self, name: str) -> Any:
        raise RuntimeError(f"torch.version.{name} is unreadable")


def test_probe_fails_closed_when_the_build_metadata_raises(driver_report: Path) -> None:
    driver_report.write_text(_DRIVER_REPORT, encoding="utf-8")
    torch = SimpleNamespace(
        __version__="2.13.0",
        version=_UnreadableBuildMetadata(),
        cuda=SimpleNamespace(is_available=lambda: True),
    )
    capability = probe_cuda_runtime(torch)
    assert capability.accelerator_runtime_built is False
    assert capability.accelerator_runtime_available is False
    assert capability.accelerator_runtime_version == "595.84"
    assert capability.unavailable_reason == protocol.REASON_ACCELERATOR_RUNTIME_UNAVAILABLE


def test_probe_fails_closed_when_the_availability_check_raises() -> None:
    def is_available() -> bool:
        raise RuntimeError("CUDA driver initialization failed")

    torch = SimpleNamespace(
        __version__="2.13.0",
        version=SimpleNamespace(cuda="12.9"),
        cuda=SimpleNamespace(is_available=is_available),
    )
    capability = probe_cuda_runtime(torch)
    assert capability.accelerator_runtime_built is True
    assert capability.accelerator_runtime_available is False
    assert capability.unavailable_reason == protocol.REASON_ACCELERATOR_RUNTIME_UNAVAILABLE
    with pytest.raises(BackendError) as caught:
        require_cuda_runtime(torch)
    assert caught.value.code == protocol.ERR_UNSUPPORTED


def _load_through_runner(tmp_path: Path) -> tuple[dict[str, Any], SidecarRunner]:
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
    finally:
        client.close()
        server.close()
    assert response is not None
    return response.header, runner


def test_default_runner_selects_cuda_fixture_from_artifact(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path, driver_report: Path
) -> None:
    driver_report.write_text(_DRIVER_REPORT, encoding="utf-8")
    calls = _install_fake_torch(monkeypatch, available=True)
    header, runner = _load_through_runner(tmp_path)
    assert header["status"] == protocol.STATUS_OK
    capability = header["runtime_capability"]
    assert capability["accelerator_runtime_available"] is True
    assert capability["accelerator_runtime_version"] == "595.84"
    assert runner.state.backend_factory_name == "cuda_fixture"
    assert calls == {"ones": 1, "matmul": 1, "sum": 1, "synchronize": 1}


def test_cuda_fixture_rejects_unavailable_runtime_before_tensor_operation(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    calls = _install_fake_torch(monkeypatch, available=False)
    header, runner = _load_through_runner(tmp_path)
    assert header["status"] == protocol.STATUS_ERROR
    assert header["error"]["code"] == protocol.ERR_UNSUPPORTED
    capability = header["runtime_capability"]
    assert capability["accelerator_runtime_available"] is False
    assert capability["unavailable_reason"] == protocol.REASON_ACCELERATOR_RUNTIME_UNAVAILABLE
    assert runner.state.backend is None
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
    header, runner = _load_through_runner(tmp_path)
    assert header["status"] == protocol.STATUS_ERROR
    assert header["error"]["code"] == protocol.ERR_LOAD_FAILED
    assert "expected" in header["error"]["message"]
    assert header["runtime_capability"]["accelerator_runtime_available"] is True
    assert runner.state.backend is None


def test_cuda_fixture_maps_a_failing_kernel_to_a_load_failure(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    calls = _install_fake_torch(
        monkeypatch,
        available=True,
        kernel_error=RuntimeError("CUDA error: an illegal memory access was encountered"),
    )
    header, runner = _load_through_runner(tmp_path)
    assert header["status"] == protocol.STATUS_ERROR
    assert header["error"]["code"] == protocol.ERR_LOAD_FAILED
    assert "tensor operation failed" in header["error"]["message"]
    assert header["runtime_capability"]["accelerator_runtime_available"] is True
    assert runner.state.backend is None
    assert calls == {"ones": 1, "matmul": 0, "sum": 0, "synchronize": 0}


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

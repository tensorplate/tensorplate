"""MPS runtime capability tests at the Python/PyTorch sidecar boundary."""

from __future__ import annotations

import contextlib
import json
import socket
import sys
import threading
import uuid
from collections.abc import Iterator
from pathlib import Path
from types import ModuleType, SimpleNamespace
from typing import Any

import pytest

from tensorplate_pytorch_backend import codec, protocol
from tensorplate_pytorch_backend.accelerator import probe_mps_runtime
from tensorplate_pytorch_backend.runner import SidecarRunner


class _FakeMps:
    def __init__(self, *, built: bool, available: bool) -> None:
        self._built = built
        self._available = available

    def is_built(self) -> bool:
        return self._built

    def is_available(self) -> bool:
        return self._available


def _module(name: str, **attributes: Any) -> ModuleType:
    module = ModuleType(name)
    for key, value in attributes.items():
        setattr(module, key, value)
    return module


def _install_fake_dependencies(
    monkeypatch: pytest.MonkeyPatch, *, runtime_available: bool
) -> dict[str, int]:
    calls = {"config": 0, "policy": 0, "tokenizer": 0}
    torch = _module(
        "torch",
        __version__="2.9.1",
        backends=SimpleNamespace(mps=_FakeMps(built=True, available=runtime_available)),
        device=lambda name: SimpleNamespace(type=name),
        cuda=SimpleNamespace(is_available=lambda: False),
    )
    monkeypatch.setitem(sys.modules, "torch", torch)
    monkeypatch.setitem(sys.modules, "numpy", _module("numpy"))

    class FakePreTrainedConfig:
        @staticmethod
        def from_pretrained(model_id: str, *, cache_dir: str) -> SimpleNamespace:
            _ = (model_id, cache_dir)
            calls["config"] += 1
            return SimpleNamespace(vlm_model_name="fake-vlm")

    class FakeSmolVLAPolicy:
        @staticmethod
        def from_pretrained(model_id: str, *, config: SimpleNamespace, cache_dir: str) -> object:
            _ = (model_id, config, cache_dir)
            calls["policy"] += 1
            return object()

    class FakeAutoTokenizer:
        @staticmethod
        def from_pretrained(model_id: str, *, cache_dir: str, padding_side: str) -> object:
            _ = (model_id, cache_dir, padding_side)
            calls["tokenizer"] += 1
            return object()

    modules = {
        "lerobot": _module("lerobot", __path__=[]),
        "lerobot.configs": _module("lerobot.configs", __path__=[]),
        "lerobot.configs.policies": _module(
            "lerobot.configs.policies", PreTrainedConfig=FakePreTrainedConfig
        ),
        "lerobot.policies": _module("lerobot.policies", __path__=[]),
        "lerobot.policies.smolvla": _module("lerobot.policies.smolvla", __path__=[]),
        "lerobot.policies.smolvla.modeling_smolvla": _module(
            "lerobot.policies.smolvla.modeling_smolvla",
            SmolVLAPolicy=FakeSmolVLAPolicy,
        ),
        "lerobot.utils": _module("lerobot.utils", __path__=[]),
        "lerobot.utils.constants": _module(
            "lerobot.utils.constants",
            OBS_LANGUAGE_ATTENTION_MASK="observation.language.attention_mask",
            OBS_LANGUAGE_TOKENS="observation.language.tokens",
            OBS_STATE="observation.state",
        ),
        "transformers": _module("transformers", AutoTokenizer=FakeAutoTokenizer),
    }
    for name, module in modules.items():
        monkeypatch.setitem(sys.modules, name, module)
    return calls


def _request(kind: str, **extra: Any) -> codec.SidecarFrame:
    header: dict[str, Any] = {
        "schema_version": protocol.SCHEMA_VERSION,
        "message_id": uuid.uuid4().hex,
        "kind": kind,
    }
    header.update(extra)
    return codec.SidecarFrame(header=header)


def _round_trip(client: socket.socket, frame: codec.SidecarFrame) -> codec.SidecarFrame:
    client.sendall(codec.encode(frame))
    buf = bytearray()
    while True:
        try:
            decoded, _consumed = codec.decode_one(bytes(buf))
        except codec.IncompleteFrame:
            chunk = client.recv(65536)
            if not chunk:
                raise AssertionError("runner closed before sending response") from None
            buf.extend(chunk)
            continue
        return decoded


@contextlib.contextmanager
def _running_sidecar() -> Iterator[tuple[socket.socket, SidecarRunner]]:
    client, server = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    runner = SidecarRunner(server, default_backend_name="smolvla")
    thread = threading.Thread(target=runner.serve_forever, daemon=True)
    thread.start()
    try:
        yield client, runner
    finally:
        with contextlib.suppress(OSError):
            client.shutdown(socket.SHUT_RDWR)
        client.close()
        thread.join(timeout=5.0)


def _model_spec(config_path: Path) -> dict[str, Any]:
    return {
        "schema_version": "0.1",
        "model_id": "smolvla-mps",
        "model_class": "vla",
        "artifact_path": str(config_path),
        "backend_hint": "python_pytorch",
        "precision_hint": "auto",
        "profile_id": "smolvla",
    }


def test_probe_captures_framework_and_runtime_versions() -> None:
    torch = SimpleNamespace(
        __version__="2.9.1",
        backends=SimpleNamespace(mps=_FakeMps(built=True, available=True)),
    )
    capability = probe_mps_runtime(torch, accelerator_runtime_version="26.0.1")
    assert capability.to_wire() == {
        "backend_name": "python_pytorch",
        "framework_version": "2.9.1",
        "accelerator_runtime_version": "26.0.1",
        "accelerator_runtime_built": True,
        "accelerator_runtime_available": True,
    }


def test_probe_fails_closed_when_framework_lacks_runtime_build() -> None:
    torch = SimpleNamespace(
        __version__="2.9.1",
        backends=SimpleNamespace(mps=_FakeMps(built=False, available=True)),
    )
    capability = probe_mps_runtime(torch, accelerator_runtime_version="26.0.1")
    assert capability.accelerator_runtime_built is False
    assert capability.accelerator_runtime_available is False
    assert capability.unavailable_reason == protocol.REASON_ACCELERATOR_RUNTIME_UNAVAILABLE


def test_available_runtime_is_recorded_on_load_and_health(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    calls = _install_fake_dependencies(monkeypatch, runtime_available=True)
    config = tmp_path / "smolvla.json"
    config.write_text(json.dumps({"device": "mps"}), encoding="utf-8")

    with _running_sidecar() as (client, _runner):
        load = _round_trip(
            client,
            _request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec(config)),
        )
        assert load.header["status"] == protocol.STATUS_OK
        capability = load.header["runtime_capability"]
        assert capability["backend_name"] == "python_pytorch"
        assert capability["framework_version"] == "2.9.1"
        assert capability["accelerator_runtime_available"] is True
        assert capability["accelerator_runtime_built"] is True
        assert capability["accelerator_runtime_version"]

        health = _round_trip(client, _request(protocol.KIND_HEALTH_CHECK))
        assert health.header["runtime_capability"] == capability

    assert calls == {"config": 1, "policy": 1, "tokenizer": 1}


def test_missing_runtime_rejects_before_model_load(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path
) -> None:
    calls = _install_fake_dependencies(monkeypatch, runtime_available=False)
    config = tmp_path / "smolvla.json"
    config.write_text(json.dumps({"device": "mps"}), encoding="utf-8")

    with _running_sidecar() as (client, runner):
        load = _round_trip(
            client,
            _request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec(config)),
        )
        assert load.header["status"] == protocol.STATUS_ERROR
        assert load.header["error"]["code"] == protocol.ERR_UNSUPPORTED
        assert protocol.REASON_ACCELERATOR_RUNTIME_UNAVAILABLE in load.header["error"]["message"]
        capability = load.header["runtime_capability"]
        assert capability["accelerator_runtime_available"] is False
        assert capability["unavailable_reason"] == protocol.REASON_ACCELERATOR_RUNTIME_UNAVAILABLE
        assert runner.state.backend is None

    assert calls == {"config": 0, "policy": 0, "tokenizer": 0}

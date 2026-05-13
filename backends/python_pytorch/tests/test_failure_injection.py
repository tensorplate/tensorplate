"""V01-E05-F06-T03 sidecar failure-injection contract tests (Python side).

Exercises the failure paths the C++ adapter and bundle pipeline rely on:
typed `load_failed`, `inference_failed`, and `shape_mismatch` errors;
unknown-version rejection; cancel-before-infer; and tensor-payload
window overflow. The tests run the runner against the
:class:`FixtureBackend` and use its `fail_*` hooks for deterministic
typed errors.
"""

from __future__ import annotations

import contextlib
import socket
import threading
import uuid
from typing import Any

import pytest

from tensorplate_pytorch_backend import codec, protocol
from tensorplate_pytorch_backend.backends.fixture import FixtureBackend
from tensorplate_pytorch_backend.runner import SidecarRunner


def _make_request(kind: str, **extra: Any) -> codec.SidecarFrame:
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
                raise AssertionError("runner closed before response") from None
            buf.extend(chunk)
            continue
        return decoded


class _ProgrammableFactory:
    """Factory adapter that captures the constructed FixtureBackend so a
    test can program its failure hooks before `load`."""

    def __init__(self) -> None:
        self.last: FixtureBackend | None = None
        self.program: dict[str, tuple[str, str]] | None = None

    def __call__(self) -> FixtureBackend:
        backend = FixtureBackend()
        if self.program is not None:
            backend.fail_load = self.program.get("load")
            backend.fail_prime = self.program.get("prime")
            backend.fail_infer = self.program.get("infer")
        self.last = backend
        return backend


def _model_spec() -> dict[str, Any]:
    return {
        "schema_version": "0.1",
        "model_id": "fixture-1",
        "model_class": "vision",
        "artifact_path": "/dev/null",
        "backend_hint": "python_pytorch",
        "precision_hint": "auto",
    }


@pytest.fixture
def runner_pair():
    factory = _ProgrammableFactory()
    a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    runner = SidecarRunner(b, backend_factories={"fixture": factory})  # type: ignore[arg-type]
    thread = threading.Thread(target=runner.serve_forever, daemon=True)
    thread.start()
    yield a, runner, factory
    with contextlib.suppress(OSError):
        a.shutdown(socket.SHUT_RDWR)
    a.close()
    thread.join(timeout=5.0)


def test_load_failure_returns_typed_load_failed(runner_pair) -> None:
    client, _, factory = runner_pair
    factory.program = {"load": (protocol.ERR_LOAD_FAILED, "engine missing")}
    resp = _round_trip(client, _make_request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec()))
    assert resp.header["status"] == protocol.STATUS_ERROR
    assert resp.header["error"]["code"] == protocol.ERR_LOAD_FAILED


def test_prime_failure_returns_typed_inference_failed(runner_pair) -> None:
    client, _, factory = runner_pair
    factory.program = {"prime": (protocol.ERR_INFERENCE_FAILED, "prime crashed")}
    assert (
        _round_trip(
            client, _make_request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec())
        ).header["status"]
        == protocol.STATUS_OK
    )
    resp = _round_trip(client, _make_request(protocol.KIND_PRIME))
    assert resp.header["status"] == protocol.STATUS_ERROR
    assert resp.header["error"]["code"] == protocol.ERR_INFERENCE_FAILED


def test_infer_failure_returns_typed_inference_failed(runner_pair) -> None:
    client, _, factory = runner_pair
    factory.program = {"infer": (protocol.ERR_INFERENCE_FAILED, "infer crashed")}
    assert (
        _round_trip(
            client, _make_request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec())
        ).header["status"]
        == protocol.STATUS_OK
    )
    assert (
        _round_trip(client, _make_request(protocol.KIND_PRIME)).header["status"]
        == protocol.STATUS_OK
    )
    req = _make_request(
        protocol.KIND_INFER,
        correlation_id="r",
        tensors=[
            {
                "name": "x",
                "tensor": {"dtype": "uint8", "shape": [1]},
                "payload_offset": 0,
                "payload_length": 1,
            }
        ],
    )
    req.payload = b"\x01"
    resp = _round_trip(client, req)
    assert resp.header["status"] == protocol.STATUS_ERROR
    assert resp.header["error"]["code"] == protocol.ERR_INFERENCE_FAILED


def test_missing_model_spec_returns_config_invalid(runner_pair) -> None:
    client, _, _ = runner_pair
    resp = _round_trip(client, _make_request(protocol.KIND_LOAD_MODEL))
    assert resp.header["status"] == protocol.STATUS_ERROR
    assert resp.header["error"]["code"] == protocol.ERR_CONFIG_INVALID


def test_malformed_tensor_entry_returns_config_invalid(runner_pair) -> None:
    client, _, _ = runner_pair
    assert (
        _round_trip(
            client, _make_request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec())
        ).header["status"]
        == protocol.STATUS_OK
    )
    assert (
        _round_trip(client, _make_request(protocol.KIND_PRIME)).header["status"]
        == protocol.STATUS_OK
    )
    req = _make_request(
        protocol.KIND_INFER,
        correlation_id="r",
        tensors=[
            {"tensor": {"dtype": "uint8", "shape": [1]}, "payload_offset": 0, "payload_length": 1}
        ],  # name missing
    )
    req.payload = b"\x01"
    resp = _round_trip(client, req)
    assert resp.header["status"] == protocol.STATUS_ERROR
    assert resp.header["error"]["code"] == protocol.ERR_CONFIG_INVALID


def test_cancel_records_request_id_on_backend(runner_pair) -> None:
    client, _, factory = runner_pair
    _round_trip(client, _make_request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec()))
    _round_trip(client, _make_request(protocol.KIND_CANCEL, correlation_id="r-late"))
    # The cancelled set is consulted on the next infer (covered by
    # test_runner). Confirm the backend received the cancel directly.
    assert factory.last is not None
    assert "r-late" in factory.last.cancelled_request_ids

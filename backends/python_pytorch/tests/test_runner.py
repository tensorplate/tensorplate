"""V01-E05-F04 end-to-end runner tests.

These tests run the :class:`SidecarRunner` against a `socketpair`-backed
client. They exercise the full request/response loop without spawning
the runner as a subprocess; the subprocess path is covered separately
in V01-E05-F05 with the C++ adapter.
"""

from __future__ import annotations

import contextlib
import socket
import threading
import uuid
from typing import Any

import pytest

from tensorplate_pytorch_backend import codec, protocol
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
                raise AssertionError("runner closed before sending response") from None
            buf.extend(chunk)
            continue
        return decoded


def _drive(client_sock: socket.socket, server_sock: socket.socket) -> SidecarRunner:
    runner = SidecarRunner(server_sock)
    thread = threading.Thread(target=runner.serve_forever, daemon=True)
    thread.start()
    # Stash the thread on the runner so tests can join it.
    runner._test_thread = thread  # type: ignore[attr-defined]
    return runner


@pytest.fixture
def runner_pair() -> tuple[socket.socket, SidecarRunner]:
    a, b = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
    runner = _drive(a, b)
    yield a, runner
    # Cleanup
    with contextlib.suppress(OSError):
        a.shutdown(socket.SHUT_RDWR)
    a.close()
    # b is owned by the runner thread; closing a forces serve_forever to exit.
    thread = runner._test_thread  # type: ignore[attr-defined]
    thread.join(timeout=5.0)


def _model_spec() -> dict[str, Any]:
    return {
        "schema_version": "0.1",
        "model_id": "fixture-1",
        "model_class": "vision",
        "artifact_path": "/dev/null",
        "backend_hint": "python_pytorch",
        "precision_hint": "auto",
    }


def test_load_prime_infer_unload_happy_path(
    runner_pair: tuple[socket.socket, SidecarRunner],
) -> None:
    client, _ = runner_pair

    load_resp = _round_trip(
        client, _make_request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec())
    )
    assert load_resp.header["kind"] == protocol.KIND_LOAD_MODEL_RESPONSE
    assert load_resp.header["status"] == protocol.STATUS_OK

    prime_resp = _round_trip(client, _make_request(protocol.KIND_PRIME))
    assert prime_resp.header["status"] == protocol.STATUS_OK

    payload = b"hello-world"
    infer_req = _make_request(
        protocol.KIND_INFER,
        correlation_id="req-1",
        tensors=[
            {
                "name": "in0",
                "tensor": {"dtype": "uint8", "shape": [len(payload)]},
                "payload_offset": 0,
                "payload_length": len(payload),
            }
        ],
    )
    infer_req.payload = payload
    infer_resp = _round_trip(client, infer_req)
    assert infer_resp.header["status"] == protocol.STATUS_OK
    assert infer_resp.header["kind"] == protocol.KIND_INFER_RESPONSE
    tensors = infer_resp.header["tensors"]
    assert len(tensors) == 1
    assert tensors[0]["name"] == "echo_in0"
    assert infer_resp.payload == payload

    unload_resp = _round_trip(client, _make_request(protocol.KIND_UNLOAD))
    assert unload_resp.header["status"] == protocol.STATUS_OK


def test_infer_before_load_returns_not_ready(
    runner_pair: tuple[socket.socket, SidecarRunner],
) -> None:
    client, _ = runner_pair
    infer_req = _make_request(
        protocol.KIND_INFER,
        correlation_id="r",
        tensors=[
            {
                "name": "in0",
                "tensor": {"dtype": "uint8", "shape": [1]},
                "payload_offset": 0,
                "payload_length": 1,
            }
        ],
    )
    infer_req.payload = b"\x00"
    resp = _round_trip(client, infer_req)
    assert resp.header["status"] == protocol.STATUS_ERROR
    assert resp.header["error"]["code"] == protocol.ERR_NOT_READY


def test_unknown_kind_returns_unsupported(runner_pair: tuple[socket.socket, SidecarRunner]) -> None:
    client, _ = runner_pair
    resp = _round_trip(client, _make_request("totally_made_up_kind"))
    assert resp.header["status"] == protocol.STATUS_ERROR
    assert resp.header["error"]["code"] == protocol.ERR_UNSUPPORTED


def test_bad_schema_version_returns_unsupported(
    runner_pair: tuple[socket.socket, SidecarRunner],
) -> None:
    client, _ = runner_pair
    bad = codec.SidecarFrame(
        header={"schema_version": "9.9", "message_id": "m", "kind": protocol.KIND_PRIME}
    )
    resp = _round_trip(client, bad)
    assert resp.header["status"] == protocol.STATUS_ERROR
    assert resp.header["error"]["code"] == protocol.ERR_UNSUPPORTED


def test_cancel_marks_pending_correlation(
    runner_pair: tuple[socket.socket, SidecarRunner],
) -> None:
    client, _ = runner_pair
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

    cancel = _make_request(protocol.KIND_CANCEL, correlation_id="r-cancel")
    assert _round_trip(client, cancel).header["status"] == protocol.STATUS_OK

    infer = _make_request(
        protocol.KIND_INFER,
        correlation_id="r-cancel",
        tensors=[
            {
                "name": "x",
                "tensor": {"dtype": "uint8", "shape": [1]},
                "payload_offset": 0,
                "payload_length": 1,
            }
        ],
    )
    infer.payload = b"\x01"
    resp = _round_trip(client, infer)
    assert resp.header["status"] == protocol.STATUS_ERROR
    assert resp.header["error"]["code"] == protocol.ERR_TIMEOUT


def test_health_check_reports_state(runner_pair: tuple[socket.socket, SidecarRunner]) -> None:
    client, _ = runner_pair
    resp = _round_trip(client, _make_request(protocol.KIND_HEALTH_CHECK))
    assert resp.header["status"] == protocol.STATUS_OK
    health = resp.header["health"]
    assert health["ready"] is False
    assert health["backend_factory"] is None

    _round_trip(client, _make_request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec()))
    resp2 = _round_trip(client, _make_request(protocol.KIND_HEALTH_CHECK))
    health2 = resp2.header["health"]
    assert health2["ready"] is True
    assert health2["backend_factory"] == "fixture"


def test_infer_async_yields_response(runner_pair: tuple[socket.socket, SidecarRunner]) -> None:
    client, _ = runner_pair
    _round_trip(client, _make_request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec()))
    _round_trip(client, _make_request(protocol.KIND_PRIME))
    req = _make_request(
        protocol.KIND_INFER_ASYNC,
        correlation_id="async-1",
        tensors=[
            {
                "name": "x",
                "tensor": {"dtype": "uint8", "shape": [4]},
                "payload_offset": 0,
                "payload_length": 4,
            }
        ],
    )
    req.payload = b"\x01\x02\x03\x04"
    resp = _round_trip(client, req)
    assert resp.header["kind"] == protocol.KIND_INFER_ASYNC_RESPONSE
    assert resp.header["status"] == protocol.STATUS_OK
    assert resp.header["async_id"] == 1
    assert resp.payload == req.payload


def test_payload_window_overflow_is_shape_mismatch(
    runner_pair: tuple[socket.socket, SidecarRunner],
) -> None:
    client, _ = runner_pair
    _round_trip(client, _make_request(protocol.KIND_LOAD_MODEL, model_spec=_model_spec()))
    _round_trip(client, _make_request(protocol.KIND_PRIME))
    req = _make_request(
        protocol.KIND_INFER,
        correlation_id="r",
        tensors=[
            {
                "name": "x",
                "tensor": {"dtype": "uint8", "shape": [10]},
                "payload_offset": 0,
                "payload_length": 10,  # but we only send 4 bytes below
            }
        ],
    )
    req.payload = b"\x01\x02\x03\x04"
    resp = _round_trip(client, req)
    assert resp.header["status"] == protocol.STATUS_ERROR
    assert resp.header["error"]["code"] == protocol.ERR_SHAPE_MISMATCH

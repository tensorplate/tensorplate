"""Integration tests for ServingClient against a fixture serving worker."""

from __future__ import annotations

import base64
import json
import struct
import threading
from collections.abc import Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from tensorplate.errors import (
    ProtocolError,
    ServingError,
    TransportError,
    UnsupportedSchemaVersionError,
)
from tensorplate.serving import (
    _BINARY_CONTENT_TYPE,
    _BINARY_INFER_MAGIC,
    _BINARY_RESULT_MAGIC,
    SCHEMA_VERSION,
    ServingClient,
    _encode_binary_infer_request,
)
from tensorplate.tensors import DType


class _CannedServer(ThreadingHTTPServer):
    routes: dict[tuple[str, str], tuple[int, object]]
    captured: list[bytes]


class _Handler(BaseHTTPRequestHandler):
    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length)
        server = self.server
        assert isinstance(server, _CannedServer)
        content_type = (self.headers.get("Content-Type") or "").split(";", 1)[0]
        if content_type == _BINARY_CONTENT_TYPE and ("POST_BINARY", self.path) not in server.routes:
            self._reply_payload(
                415,
                {
                    "schema_version": "0.1",
                    "request_id": "",
                    "status": "failure",
                    "error": {
                        "schema_version": "0.1",
                        "code": "unsupported",
                        "message": "binary transport unsupported",
                    },
                },
            )
            return
        server.captured.append(body)
        key = (
            ("POST_BINARY", self.path)
            if content_type == _BINARY_CONTENT_TYPE
            else ("POST", self.path)
        )
        self._reply(key)

    def do_GET(self) -> None:
        self._reply(("GET", self.path))

    def _reply(self, key: tuple[str, str]) -> None:
        server = self.server
        assert isinstance(server, _CannedServer)
        status, payload = server.routes.get(key, (404, {"status": "failure"}))
        self._reply_payload(status, payload)

    def _reply_payload(self, status: int, payload: object) -> None:
        if isinstance(payload, bytes):
            data = payload
            content_type = _BINARY_CONTENT_TYPE
        else:
            data = json.dumps(payload).encode("utf-8")
            content_type = "application/json"
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, format: str, *args: object) -> None:
        return


@pytest.fixture
def server() -> Iterator[_CannedServer]:
    httpd = _CannedServer(("127.0.0.1", 0), _Handler)
    httpd.routes = {}
    httpd.captured = []
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    try:
        yield httpd
    finally:
        httpd.shutdown()
        httpd.server_close()
        thread.join(timeout=2)


def _base_url(httpd: _CannedServer) -> str:
    address = httpd.server_address
    assert isinstance(address, tuple)
    return f"http://127.0.0.1:{address[1]}"


def _client(httpd: _CannedServer) -> ServingClient:
    return ServingClient(_base_url(httpd), discover=False, timeout=3.0)


def _success_body() -> dict[str, object]:
    return {
        "schema_version": "0.1",
        "request_id": "r-1",
        "status": "success",
        "outputs": [
            {
                "name": "out",
                "tensor": {"dtype": "float32", "shape": [2], "byte_size": 8},
                "payload_b64": base64.b64encode(b"\x00\x00\x80?\x00\x00\x00@").decode("ascii"),
                "semantic_tag": "detections",
            }
        ],
    }


def _binary_success_body() -> bytes:
    payload = b"\x00\x00\x80?\x00\x00\x00@"
    metadata = {
        "schema_version": "0.1",
        "request_id": "r-bin",
        "status": "success",
        "outputs": [
            {
                "name": "out",
                "tensor": {"dtype": "float32", "shape": [2], "byte_size": 8},
                "payload_offset": 0,
                "payload_size": len(payload),
                "semantic_tag": "detections",
            }
        ],
    }
    encoded = json.dumps(metadata, separators=(",", ":")).encode("utf-8")
    return _BINARY_RESULT_MAGIC + struct.pack("<I", len(encoded)) + encoded + payload


def test_infer_success_and_request_shape(server: _CannedServer) -> None:
    server.routes[("POST", "/infer")] = (200, _success_body())
    result = _client(server).infer(
        "model", [ServingClient.tensor_input("x", b"\x00\x00\x80?", DType.FLOAT32, (1,))]
    )
    assert result.request_id == "r-1"
    out = result.output("out")
    assert out.shape == (2,)
    assert out.semantic_tag == "detections"

    sent = json.loads(server.captured[0])
    assert sent["schema_version"] == "0.1"
    assert sent["endpoint"] == "model"
    assert sent["request_id"]
    assert sent["inputs"][0]["name"] == "x"
    assert "payload_b64" in sent["inputs"][0]
    assert result.transport == "json"


def test_infer_rejects_invalid_request_fields(server: _CannedServer) -> None:
    client = _client(server)
    tensor = ServingClient.tensor_input("x", b"\x00", DType.UINT8, (1,))

    with pytest.raises(ValueError, match="endpoint"):
        client.infer("", [tensor])
    with pytest.raises(ValueError, match="deadline_ms"):
        client.infer("model", [tensor], deadline_ms=0)
    with pytest.raises(ValueError, match="correlation_id"):
        client.infer("model", [tensor], correlation_id="")
    assert server.captured == []


def test_infer_failure_maps_to_serving_error(server: _CannedServer) -> None:
    server.routes[("POST", "/infer")] = (
        200,
        {
            "schema_version": "0.1",
            "request_id": "r",
            "status": "failure",
            "error": {"schema_version": "0.1", "code": "shape_mismatch", "message": "bad shape"},
        },
    )
    with pytest.raises(ServingError) as excinfo:
        _client(server).infer("m", [ServingClient.tensor_input("x", b"\x00", DType.UINT8, (1,))])
    assert excinfo.value.code.value == "shape_mismatch"
    assert excinfo.value.request_id == "r"


def test_infer_rejects_unsupported_schema_version(server: _CannedServer) -> None:
    server.routes[("POST", "/infer")] = (
        200,
        {"schema_version": "0.2", "status": "success", "outputs": []},
    )
    with pytest.raises(UnsupportedSchemaVersionError):
        _client(server).infer("m", [ServingClient.tensor_input("x", b"\x00", DType.UINT8, (1,))])


@pytest.mark.parametrize(
    "body",
    [
        {"schema_version": "0.1", "request_id": "r-1", "status": "success", "outputs": []},
        {
            "schema_version": "0.1",
            "status": "success",
            "outputs": _success_body()["outputs"],
        },
    ],
)
def test_infer_rejects_malformed_success_response(
    server: _CannedServer, body: dict[str, object]
) -> None:
    server.routes[("POST", "/infer")] = (200, body)
    with pytest.raises(ProtocolError):
        _client(server).infer("m", [ServingClient.tensor_input("x", b"\x00", DType.UINT8, (1,))])


def test_infer_transport_error_on_dead_endpoint() -> None:
    client = ServingClient("http://127.0.0.1:1/infer", discover=False, timeout=1.0)
    with pytest.raises(TransportError):
        client.infer("m", [ServingClient.tensor_input("x", b"\x00", DType.UINT8, (1,))])


def test_health_ready(server: _CannedServer) -> None:
    server.routes[("GET", "/health")] = (
        200,
        {
            "schema_version": "0.1",
            "state": "ready",
            "endpoint": "http://127.0.0.1:18080",
            "backend": "trt",
        },
    )
    snapshot = _client(server).health()
    assert snapshot.is_ready
    assert snapshot.backend == "trt"


def test_health_degraded_returns_state_not_just_http(server: _CannedServer) -> None:
    server.routes[("GET", "/health")] = (
        503,
        {
            "schema_version": "0.1",
            "state": "failed",
            "endpoint": "x",
            "backend": "trt",
            "last_error_code": "load_failed",
            "last_error_message": "boom",
        },
    )
    snapshot = _client(server).health()
    assert not snapshot.is_ready
    assert snapshot.state == "failed"
    assert snapshot.last_error_code is not None
    assert snapshot.last_error_code.value == "load_failed"


def test_round_trips_against_unchanged_v0_1_2_worker(server: _CannedServer) -> None:
    # The /infer and /health envelope is schema_version "0.1", pinned
    # unchanged since v0.1.2, so the SDK works against an unchanged v0.1.2
    # serving worker without any code path difference.
    assert SCHEMA_VERSION == "0.1"
    server.routes[("GET", "/health")] = (
        200,
        {"schema_version": "0.1", "state": "ready", "endpoint": "default", "backend": "trt"},
    )
    server.routes[("POST", "/infer")] = (200, _success_body())
    client = _client(server)
    assert client.health().is_ready
    result = client.infer(
        "m", [ServingClient.tensor_input("x", b"\x00\x00\x80?", DType.FLOAT32, (1,))]
    )
    assert result.request_id == "r-1"
    sent = json.loads(server.captured[0])
    assert sent["schema_version"] == "0.1"


def test_binary_request_encoder_uses_raw_payload() -> None:
    wire = _encode_binary_infer_request(
        "r",
        "model",
        [ServingClient.tensor_input("x", b"\x01\x02\x03\x04", DType.UINT8, (4,))],
    )
    assert wire.startswith(_BINARY_INFER_MAGIC)
    (metadata_len,) = struct.unpack(
        "<I", wire[len(_BINARY_INFER_MAGIC) : len(_BINARY_INFER_MAGIC) + 4]
    )
    metadata_start = len(_BINARY_INFER_MAGIC) + 4
    metadata = json.loads(wire[metadata_start : metadata_start + metadata_len])
    assert metadata["inputs"][0]["payload_offset"] == 0
    assert metadata["inputs"][0]["payload_size"] == 4
    assert wire[-4:] == b"\x01\x02\x03\x04"


def test_binary_response_parser_reconstructs_outputs(server: _CannedServer) -> None:
    server.routes[("POST_BINARY", "/infer")] = (200, _binary_success_body())
    result = ServingClient(
        _base_url(server), discover=False, timeout=3.0, preferred_transport="binary"
    ).infer("model", [ServingClient.tensor_input("x", b"\x00", DType.UINT8, (1,))])
    assert result.transport == "binary"
    assert result.request_id == "r-bin"
    assert result.output("out").data == b"\x00\x00\x80?\x00\x00\x00@"
    assert server.captured[0].startswith(_BINARY_INFER_MAGIC)


def test_auto_falls_back_to_json_and_caches_decision(server: _CannedServer) -> None:
    server.routes[("POST", "/infer")] = (200, _success_body())
    client = _client(server)
    first = client.infer("m", [ServingClient.tensor_input("x", b"\x00", DType.UINT8, (1,))])
    second = client.infer("m", [ServingClient.tensor_input("x", b"\x00", DType.UINT8, (1,))])
    assert first.transport == "json"
    assert second.transport == "json"
    assert len(server.captured) == 2
    assert all(json.loads(body)["schema_version"] == "0.1" for body in server.captured)


def test_binary_mode_does_not_fallback(server: _CannedServer) -> None:
    server.routes[("POST", "/infer")] = (200, _success_body())
    client = ServingClient(
        _base_url(server), discover=False, timeout=3.0, preferred_transport="binary"
    )
    with pytest.raises(ServingError) as excinfo:
        client.infer("m", [ServingClient.tensor_input("x", b"\x00", DType.UINT8, (1,))])
    assert excinfo.value.code.value == "unsupported"
    assert server.captured == []


def test_json_mode_preserves_json_request_shape(server: _CannedServer) -> None:
    server.routes[("POST", "/infer")] = (200, _success_body())
    result = ServingClient(
        _base_url(server), discover=False, timeout=3.0, preferred_transport="json"
    ).infer("m", [ServingClient.tensor_input("x", b"\x00", DType.UINT8, (1,))])
    assert result.transport == "json"
    assert len(server.captured) == 1
    assert json.loads(server.captured[0])["inputs"][0]["payload_b64"] == "AA=="


def _unsupported_failure_body(message: str) -> dict[str, object]:
    return {
        "schema_version": "0.1",
        "request_id": "r-bad",
        "status": "failure",
        "error": {
            "schema_version": "0.1",
            "code": "unsupported",
            "message": message,
        },
    }


def test_auto_keeps_binary_after_request_content_415(server: _CannedServer) -> None:
    # A 415 'unsupported' caused by request content (unknown dtype/layout,
    # schema_version, ...) rather than a missing binary transport: the worker
    # returns the same status+code over JSON, so the error must propagate and
    # the client must NOT latch binary off — otherwise one bad request would
    # permanently downgrade the connection to JSON (py-bin-1 regression).
    server.routes[("POST", "/infer")] = (
        415,
        _unsupported_failure_body("unknown dtype 'weird'"),
    )
    client = _client(server)
    with pytest.raises(ServingError) as excinfo:
        client.infer("m", [ServingClient.tensor_input("x", b"\x00", DType.UINT8, (1,))])
    assert excinfo.value.code.value == "unsupported"
    assert client._binary_supported is None

    # Binary is still attempted on the next call and succeeds.
    server.routes[("POST_BINARY", "/infer")] = (200, _binary_success_body())
    result = client.infer("m", [ServingClient.tensor_input("x", b"\x00", DType.UINT8, (1,))])
    assert result.transport == "binary"
    assert client._binary_supported is True


@pytest.mark.parametrize(
    "corrupt",
    [
        _BINARY_RESULT_MAGIC,  # header without the metadata-length field
        _BINARY_RESULT_MAGIC + struct.pack("<I", 999),  # metadata longer than body
        _BINARY_RESULT_MAGIC + struct.pack("<I", 3) + b"{x}",  # malformed metadata JSON
    ],
)
def test_binary_response_parser_rejects_malformed(server: _CannedServer, corrupt: bytes) -> None:
    server.routes[("POST_BINARY", "/infer")] = (200, corrupt)
    client = ServingClient(
        _base_url(server), discover=False, timeout=3.0, preferred_transport="binary"
    )
    with pytest.raises(ProtocolError):
        client.infer("m", [ServingClient.tensor_input("x", b"\x00", DType.UINT8, (1,))])

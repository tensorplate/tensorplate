"""Integration tests for ServingClient against a fixture serving worker."""

from __future__ import annotations

import base64
import json
import threading
from collections.abc import Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from tensorplate.errors import (
    ServingError,
    TransportError,
    UnsupportedSchemaVersionError,
)
from tensorplate.serving import ServingClient
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
        server.captured.append(body)
        self._reply(("POST", self.path))

    def do_GET(self) -> None:
        self._reply(("GET", self.path))

    def _reply(self, key: tuple[str, str]) -> None:
        server = self.server
        assert isinstance(server, _CannedServer)
        status, payload = server.routes.get(key, (404, {"status": "failure"}))
        data = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
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

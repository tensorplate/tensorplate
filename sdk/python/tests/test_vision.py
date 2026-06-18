"""Tests for the high-level VisionClient.detect composition."""

from __future__ import annotations

import base64
import json
import threading
from collections.abc import Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from tensorplate.conventions import detections
from tensorplate.errors import ProtocolError
from tensorplate.preprocess import PreprocessConfig
from tensorplate.serving import InferResult
from tensorplate.tensors import DType, TensorOutput
from tensorplate.vision import VisionClient, _select_detection_output


def _output(name: str, semantic_tag: str | None = None) -> TensorOutput:
    return TensorOutput(name, DType.FLOAT32, (1,), b"\x00\x00\x00\x00", semantic_tag=semantic_tag)


def test_select_single_output() -> None:
    result = InferResult(request_id="r", outputs=(_output("y"),))
    assert _select_detection_output(result, None).name == "y"


def test_select_explicit_output_name() -> None:
    result = InferResult(request_id="r", outputs=(_output("a"), _output("b")))
    assert _select_detection_output(result, "b").name == "b"


def test_select_explicit_missing_fails_clearly() -> None:
    result = InferResult(request_id="r", outputs=(_output("a"),))
    with pytest.raises(ProtocolError, match="no output named"):
        _select_detection_output(result, "missing")


def test_select_by_semantic_tag() -> None:
    result = InferResult(
        request_id="r", outputs=(_output("a"), _output("b", semantic_tag=detections.boxes))
    )
    assert _select_detection_output(result, None).name == "b"


def test_select_ambiguous_fails_clearly() -> None:
    result = InferResult(request_id="r", outputs=(_output("a"), _output("b")))
    with pytest.raises(ProtocolError, match="output_name"):
        _select_detection_output(result, None)


class _CannedServer(ThreadingHTTPServer):
    routes: dict[tuple[str, str], tuple[int, object]]
    captured: list[bytes]


class _Handler(BaseHTTPRequestHandler):
    def do_POST(self) -> None:
        body = self.rfile.read(int(self.headers.get("Content-Length") or 0))
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


def test_detect_end_to_end(server: _CannedServer) -> None:
    pytest.importorskip("numpy")
    pytest.importorskip("PIL")
    import numpy

    # YOLO [1, 4+1, 1] output: one box (cx,cy,w,h) + 1 class score, in 320x320 model pixels.
    grid = numpy.array([[160], [160], [80], [80], [0.9]], dtype=numpy.float32).reshape(1, 5, 1)
    body = {
        "schema_version": "0.1",
        "request_id": "r",
        "status": "success",
        "outputs": [
            {
                "name": "out",
                "tensor": {"dtype": "float32", "shape": [1, 5, 1], "byte_size": 20},
                "payload_b64": base64.b64encode(grid.tobytes()).decode("ascii"),
            }
        ],
    }
    server.routes[("POST", "/infer")] = (200, body)

    client = VisionClient(_base_url(server), discover=False, timeout=3.0)
    image = numpy.zeros((90, 160, 3), dtype=numpy.uint8)  # source 90h x 160w
    dets = client.detect(
        image,
        endpoint="m",
        input_name="custom_images",
        score_threshold=0.25,
        labels=["obj"],
        preprocess_config=PreprocessConfig(input_size=(320, 320)),
    )

    assert len(dets) == 1
    assert dets[0].label == "obj"
    assert dets[0].class_id == 0
    assert dets[0].score == pytest.approx(0.9)
    # letterbox: scale 2.0, pad_y 70 -> model box (120,120,200,200) maps to source pixels
    assert dets[0].box == pytest.approx((60.0, 25.0, 100.0, 65.0))
    sent = json.loads(server.captured[0])
    assert sent["inputs"][0]["name"] == "custom_images"

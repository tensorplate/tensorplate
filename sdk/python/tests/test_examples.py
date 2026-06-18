"""Smoke tests for the vision_detection_sdk example scripts.

The ``--help`` smokes run anywhere (they need only the core SDK). The
end-to-end check runs the example as a subprocess against an in-process
fixture worker and requires the vision extras.
"""

from __future__ import annotations

import base64
import json
import subprocess
import sys
import threading
from collections.abc import Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

_EXAMPLES = Path(__file__).resolve().parents[3] / "examples" / "vision_detection_sdk"
_YOLO = _EXAMPLES / "yolo_detect.py"
_CAMERA = _EXAMPLES / "camera_infer.py"


def _run(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, *args],
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )


def test_yolo_detect_help() -> None:
    result = _run(str(_YOLO), "--help")
    assert result.returncode == 0
    assert "--endpoint" in result.stdout


def test_camera_infer_help() -> None:
    result = _run(str(_CAMERA), "--help")
    assert result.returncode == 0
    assert "--source" in result.stdout


class _CannedServer(ThreadingHTTPServer):
    routes: dict[tuple[str, str], tuple[int, object]]


class _Handler(BaseHTTPRequestHandler):
    def do_POST(self) -> None:
        self.rfile.read(int(self.headers.get("Content-Length") or 0))
        server = self.server
        assert isinstance(server, _CannedServer)
        status, payload = server.routes.get(("POST", self.path), (404, {"status": "failure"}))
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
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    try:
        yield httpd
    finally:
        httpd.shutdown()
        httpd.server_close()
        thread.join(timeout=2)


def test_yolo_detect_against_fixture(server: _CannedServer, tmp_path: Path) -> None:
    pytest.importorskip("numpy")
    pytest.importorskip("PIL")
    import numpy
    from PIL import Image

    grid = numpy.array([[160], [160], [80], [80], [0.9]], dtype=numpy.float32).reshape(1, 5, 1)
    server.routes[("POST", "/infer")] = (
        200,
        {
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
        },
    )
    image_path = tmp_path / "frame.png"
    Image.fromarray(numpy.zeros((90, 160, 3), dtype=numpy.uint8)).save(image_path)
    address = server.server_address
    assert isinstance(address, tuple)
    base_url = f"http://127.0.0.1:{address[1]}"

    result = _run(
        str(_YOLO),
        "--serving-url",
        base_url,
        "--endpoint",
        "m",
        "--image",
        str(image_path),
        "--input-size",
        "320",
        "--json",
    )
    assert result.returncode == 0, result.stderr
    detections = json.loads(result.stdout)
    assert len(detections) == 1
    assert detections[0]["class_id"] == 0
    assert detections[0]["score"] == pytest.approx(0.9)
    assert detections[0]["box"] == pytest.approx([60.0, 25.0, 100.0, 65.0])

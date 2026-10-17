# SPDX-License-Identifier: Apache-2.0
"""Real worker/fixture-sidecar transport proof in an NVIDIA-free container."""

import importlib.metadata
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

import tensorplate_pytorch_backend  # noqa: F401 -- mandatory, never a skipped proof


def request(port, path, body=None, timeout=2):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(
        f"http://127.0.0.1:{port}{path}", data, {"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(req, timeout=timeout) as response:
        return json.load(response)


worker = str(Path(sys.argv[1]).resolve())
subprocess.run([worker, "--version"], check=True, timeout=10)
assert not any(
    d.metadata["Name"].lower().startswith(("nvidia", "torch"))
    for d in importlib.metadata.distributions()
)
cache = subprocess.check_output(["ldconfig", "-p"], text=True)
assert not any(name in cache for name in ("libcuda", "libnvinfer", "libcublas", "libcudnn"))
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    engine = root / "invalid.engine"
    engine.write_bytes(b"non-empty invalid TensorRT engine")
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    config = {
        "schema_version": "0.1",
        "bind": {"host": "127.0.0.1", "port": port, "allow_non_loopback": False},
        "enable_stderr_logs": True,
        "deployment": {
            "use_mock_session": False, "endpoint": "default", "backend": "python_pytorch",
            "model": {"model_id": "fixture", "model_class": "vla",
                      "artifact_path": "/dev/null", "backend_hint": "python_pytorch"},
        },
    }
    env = {**os.environ, "TP_TEST_PYTHON_EXE": sys.executable}
    with (root / "worker.log").open("w+") as log:
        process = subprocess.Popen(
            [worker, "--config-json", json.dumps(config)], env=env,
            stdout=log, stderr=log, start_new_session=True,
        )
        try:
            deadline = time.monotonic() + 30
            while True:
                assert process.poll() is None, "worker exited before readiness"
                try:
                    if request(port, "/health")["state"] == "ready":
                        break
                except (OSError, urllib.error.URLError):
                    pass
                assert time.monotonic() < deadline, "worker never became ready"
                time.sleep(0.05)
            result = request(port, "/infer", {
                "schema_version": "0.1", "request_id": "real-python", "endpoint": "default",
                "inputs": [{"name": "image", "tensor": {
                    "dtype": "uint8", "shape": [2, 2], "byte_size": 4},
                    "payload_b64": "AAECAw=="}],
            }, timeout=30)
            assert result["outputs"][0]["name"] == "echo_image", result
            assert result["outputs"][0]["payload_b64"] == "AAECAw==", result
            maps = Path(f"/proc/{process.pid}/maps").read_text()
            assert not any(name in maps for name in ("libcuda", "libnvinfer", "libcublas"))
        finally:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=10)
            finally:
                # Bound cleanup even if a sidecar outlives its worker.
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait(timeout=5)
                log.seek(0)
                print(log.read())
    config["deployment"]["backend"] = "tensorrt"
    config["deployment"]["model"].update(
        model_class="vision", artifact_path=str(engine), backend_hint="tensorrt"
    )
    refused = subprocess.run([worker, "--config-json", json.dumps(config)],
                             capture_output=True, text=True, timeout=30, env=env)
    assert refused.returncode == 65, refused.stderr
    failure = json.loads(refused.stderr.splitlines()[-1])["fields"]
    assert failure["code"] == "unsupported", failure
    assert "cannot load libcudart.so." in failure["message"], failure
print("PASS: real Python response and TensorRT Unsupported without NVIDIA libraries")

"""Opt-in end-to-end test against a real ``tensorplate-serving`` worker.

Set ``TENSORPLATE_SERVING_WORKER_BIN`` to the worker binary to enable this
test. It launches the worker in ``--mock`` mode (no GPU) on a free loopback
port and round-trips the SDK against it. It is skipped by default, so the
always-on fixture-worker tests in ``test_serving.py`` remain the CI gate.
"""

from __future__ import annotations

import os
import socket
import subprocess
import time
from collections.abc import Iterator

import pytest

from tensorplate.serving import SCHEMA_VERSION, ServingClient
from tensorplate.tensors import DType

_WORKER_BIN_ENV = "TENSORPLATE_SERVING_WORKER_BIN"


def _free_port() -> int:
    sock = socket.socket()
    try:
        sock.bind(("127.0.0.1", 0))
        port: int = sock.getsockname()[1]
        return port
    finally:
        sock.close()


@pytest.fixture
def worker() -> Iterator[ServingClient]:
    binary = os.environ.get(_WORKER_BIN_ENV)
    if not binary:
        pytest.skip(f"set {_WORKER_BIN_ENV} to run the real-worker e2e test")
    port = _free_port()
    proc = subprocess.Popen(
        [binary, "--mock", "--bind-host", "127.0.0.1", "--bind-port", str(port)]
    )
    client = ServingClient(f"http://127.0.0.1:{port}", discover=False, timeout=5.0)
    try:
        deadline = time.monotonic() + 10.0
        while True:
            try:
                client.health()
                break
            except Exception:  # worker is still starting up; retry until the deadline
                if time.monotonic() > deadline:
                    proc.terminate()
                    pytest.fail("serving worker did not become reachable")
                time.sleep(0.1)
        yield client
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()


def test_health_reports_ready(worker: ServingClient) -> None:
    snapshot = worker.health()
    assert snapshot.is_ready
    # The SDK only speaks schema_version 0.1, which the worker has emitted
    # unchanged since v0.1.2.
    assert SCHEMA_VERSION == "0.1"


def test_infer_round_trip(worker: ServingClient) -> None:
    result = worker.infer(
        "default",
        [ServingClient.tensor_input("image", b"\x00\x00\x00\x00", DType.UINT8, (2, 2))],
    )
    assert result.request_id
    assert result.outputs
    assert result.outputs[0].name == "actions"

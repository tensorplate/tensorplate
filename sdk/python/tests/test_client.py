"""Tests for serving endpoint resolution and URL canonicalization."""

from __future__ import annotations

import json
import os
import shutil
import socket
import tempfile
import threading
from pathlib import Path

import pytest

from tensorplate.client import canonicalize_serving_url, resolve_serving_url
from tensorplate.errors import EndpointResolutionError


@pytest.mark.parametrize(
    ("value", "url", "path"),
    [
        ("http://127.0.0.1:18080", "http://127.0.0.1:18080/infer", "/infer"),
        ("http://127.0.0.1:18080/infer", "http://127.0.0.1:18080/infer", "/infer"),
        ("http://127.0.0.1:18080/", "http://127.0.0.1:18080/infer", "/infer"),
        ("http://10.0.0.5:30000/v1/predict", "http://10.0.0.5:30000/v1/predict", "/v1/predict"),
        ("http://example", "http://example:80/infer", "/infer"),
    ],
)
def test_canonicalize_matches_cli(value: str, url: str, path: str) -> None:
    endpoint = canonicalize_serving_url(value, "explicit")
    assert endpoint.url == url
    assert endpoint.path == path
    assert endpoint.source == "explicit"


@pytest.mark.parametrize(
    "value",
    ["https://h:1/infer", "tcp://h:1", "127.0.0.1:18080", "http://:18080", "http://h:bad"],
)
def test_canonicalize_rejects_invalid(value: str) -> None:
    with pytest.raises(EndpointResolutionError):
        canonicalize_serving_url(value, "explicit")


def test_resolve_explicit_url_wins() -> None:
    endpoint = resolve_serving_url("http://10.0.0.5:9/infer", discover=False)
    assert endpoint.source == "explicit"
    assert (endpoint.host, endpoint.port) == ("10.0.0.5", 9)


def test_resolve_profile_serving_url(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("TENSORPLATE_CLI_CONFIG", raising=False)
    config = {
        "schema_version": "0.1",
        "default_profile": "edge",
        "profiles": {
            "edge": {
                "mode": "url",
                "agent_url": "127.0.0.1:18000",
                "serving_url": "http://127.0.0.1:18080",
            }
        },
    }
    config_file = tmp_path / "cli.json"
    config_file.write_text(json.dumps(config), encoding="utf-8")
    endpoint = resolve_serving_url(None, config_path=str(config_file), discover=False)
    assert endpoint.source == "profile"
    assert endpoint.url == "http://127.0.0.1:18080/infer"


def test_resolve_falls_back_to_loopback(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("TENSORPLATE_CLI_CONFIG", raising=False)
    endpoint = resolve_serving_url(None, discover=False)
    assert endpoint.source == "loopback"
    assert endpoint.url == "http://127.0.0.1:18080/infer"


@pytest.mark.skipif(not hasattr(socket, "AF_UNIX"), reason="requires AF_UNIX")
def test_resolve_agent_discovered(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("TENSORPLATE_CLI_CONFIG", raising=False)
    # AF_UNIX paths are length-limited (~104 bytes); pytest's tmp_path is too
    # long on macOS, so bind the socket under a short directory instead.
    socket_dir = tempfile.mkdtemp(prefix="tp-agent", dir="/tmp")
    socket_path = os.path.join(socket_dir, "a.sock")
    discovered_url = "http://127.0.0.1:18081/infer"
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(socket_path)
    server.listen(1)

    def serve() -> None:
        try:
            conn, _ = server.accept()
        except OSError:
            return
        with conn:
            conn.recv(65536)
            response = {
                "schema_version": "0.1",
                "status": "ok",
                "agent_status": {
                    "agent_state": "ready",
                    "active": {
                        "deployment_id": "d",
                        "bundle_digest": "sha256:ab",
                        "serving_url": discovered_url,
                    },
                },
            }
            conn.sendall(json.dumps(response).encode("utf-8") + b"\n")

    thread = threading.Thread(target=serve, daemon=True)
    thread.start()
    try:
        config = {
            "schema_version": "0.1",
            "default_profile": "local",
            "profiles": {"local": {"mode": "local", "socket_path": socket_path}},
        }
        config_file = tmp_path / "cli.json"
        config_file.write_text(json.dumps(config), encoding="utf-8")
        endpoint = resolve_serving_url(
            None, config_path=str(config_file), discover=True, timeout=3.0
        )
    finally:
        server.close()
        thread.join(timeout=2)
        shutil.rmtree(socket_dir, ignore_errors=True)
    assert endpoint.source == "agent-discovered"
    assert endpoint.url == discovered_url


def test_resolve_unknown_profile_raises(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.delenv("TENSORPLATE_CLI_CONFIG", raising=False)
    config_file = tmp_path / "cli.json"
    config_file.write_text(json.dumps({"schema_version": "0.1"}), encoding="utf-8")
    with pytest.raises(EndpointResolutionError):
        resolve_serving_url(None, profile="nope", config_path=str(config_file), discover=False)

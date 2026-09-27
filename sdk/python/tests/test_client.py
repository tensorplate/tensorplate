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

import tensorplate
from tensorplate import ServingClient, VisionClient
from tensorplate.client import canonicalize_serving_url, resolve_serving_url
from tensorplate.errors import EndpointResolutionError, EndpointUnavailableError


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


# ---- Discovery against an agent that reports a resident set -----------------


def _member(deployment_id: str, generation: int, **fields: object) -> dict[str, object]:
    member: dict[str, object] = {
        "deployment_id": deployment_id,
        "generation": generation,
        "bundle_digest": "sha256:ab",
        "state": "serving",
        "admission_mode": "production",
        "quota": {"session_count": 0, "domain_bytes": {}},
    }
    member.update(fields)
    return member


def _set(*members: dict[str, object]) -> dict[str, object]:
    return {"set_id": "set-1", "revision": 2, "members": list(members)}


class _FakeAgent:
    """Stands in for the agent's status round trip and counts the calls."""

    def __init__(self, reply: bytes | Exception) -> None:
        self.reply = reply
        self.calls = 0

    def __call__(self, transport: object, timeout: float) -> bytes:
        self.calls += 1
        if isinstance(self.reply, Exception):
            raise self.reply
        return self.reply


def _agent_replying(
    monkeypatch: pytest.MonkeyPatch, reply: dict[str, object] | bytes | Exception
) -> _FakeAgent:
    """Answer discovery on the default local profile with ``reply``: an
    ``agent_status`` object, raw bytes, or an exception to raise."""
    monkeypatch.delenv("TENSORPLATE_CLI_CONFIG", raising=False)
    if isinstance(reply, dict):
        envelope = {"schema_version": "0.1", "status": "ok", "agent_status": reply}
        reply = json.dumps(envelope).encode("utf-8") + b"\n"
    fake = _FakeAgent(reply)
    monkeypatch.setattr("tensorplate.client._agent_status_roundtrip", fake)
    return fake


_STREAM_ONLY_ACTIVE: dict[str, object] = {
    "deployment_id": "vision-v2",
    "bundle_digest": "sha256:ab",
}
_TWO_MEMBERS: dict[str, object] = {
    "agent_state": "ready",
    "resident_set": _set(
        _member("speech-stt", 3, stream_endpoint="127.0.0.1:18103"),
        _member("speech-tts", 5, stream_endpoint="127.0.0.1:18105"),
    ),
}


def test_discovery_uses_the_serving_url_a_set_of_one_projects(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Control: a set of one serving member with a loopback unary endpoint.
    _agent_replying(
        monkeypatch,
        {
            "agent_state": "ready",
            "active": {**_STREAM_ONLY_ACTIVE, "serving_url": "http://127.0.0.1:18090/infer"},
            "resident_set": _set(_member("vision-v2", 2, unary_endpoint="http://127.0.0.1:18090")),
        },
    )
    endpoint = resolve_serving_url(None, discover=True)
    assert endpoint.source == "agent-discovered"
    assert endpoint.url == "http://127.0.0.1:18090/infer"


@pytest.mark.parametrize(
    ("agent_status", "says"),
    [
        pytest.param(
            {
                "agent_state": "ready",
                "active": _STREAM_ONLY_ACTIVE,
                "resident_set": _set(_member("vision-v2", 2, stream_endpoint="127.0.0.1:18105")),
            },
            "no loopback serving URL",
            id="stream-only-set-of-one",
        ),
        pytest.param(
            {
                "agent_state": "ready",
                "active": _STREAM_ONLY_ACTIVE,
                "resident_set": _set(
                    _member("vision-v2", 2, unary_endpoint="http://192.0.2.10:18090")
                ),
            },
            "no loopback serving URL",
            id="off-loopback-set-of-one",
        ),
        pytest.param(
            {
                "agent_state": "ready",
                "resident_set": _set(_member("vision-v2", 2, state="quarantined")),
            },
            "no loopback serving URL",
            id="quarantined-set-of-one",
        ),
        pytest.param(_TWO_MEMBERS, "resident set of 2 members", id="two-stream-members"),
        pytest.param(
            {
                "agent_state": "ready",
                "resident_set": _set(
                    _member("vision-a", 2, unary_endpoint="http://127.0.0.1:18090"),
                    _member("vision-b", 4, unary_endpoint="http://127.0.0.1:18091"),
                ),
            },
            "resident set of 2 members",
            id="two-unary-members",
        ),
        pytest.param(
            {"agent_state": "ready", "resident_set": _set()},
            "no loopback serving URL",
            id="empty-set",
        ),
        pytest.param(
            {"agent_state": "ready", "resident_set": []},
            "no loopback serving URL",
            id="malformed-set-list",
        ),
        pytest.param(
            {"agent_state": "ready", "resident_set": "x"},
            "no loopback serving URL",
            id="malformed-set-string",
        ),
        pytest.param(
            {"agent_state": "ready", "resident_set": {"set_id": "s", "members": {}}},
            "no loopback serving URL",
            id="malformed-members",
        ),
    ],
)
def test_discovery_refuses_a_resident_set_without_a_serving_url(
    monkeypatch: pytest.MonkeyPatch, agent_status: dict[str, object], says: str
) -> None:
    agent = _agent_replying(monkeypatch, agent_status)
    with pytest.raises(EndpointUnavailableError) as raised:
        resolve_serving_url(None, discover=True)
    assert isinstance(raised.value, EndpointResolutionError)
    assert says in str(raised.value)
    assert agent.calls == 1


@pytest.mark.parametrize(
    "agent_status",
    [
        pytest.param({"agent_state": "ready", "active": _STREAM_ONLY_ACTIVE}, id="no-serving-url"),
        pytest.param({"agent_state": "ready"}, id="no-active"),
        pytest.param(
            {"agent_state": "ready", "active": _STREAM_ONLY_ACTIVE, "resident_set": None},
            id="null-set",
        ),
    ],
)
def test_discovery_of_a_status_without_a_set_keeps_the_loopback_default(
    monkeypatch: pytest.MonkeyPatch, agent_status: dict[str, object]
) -> None:
    # Control: the legacy fallback is unchanged.
    _agent_replying(monkeypatch, agent_status)
    endpoint = resolve_serving_url(None, discover=True)
    assert endpoint.source == "loopback"
    assert endpoint.url == "http://127.0.0.1:18080/infer"


@pytest.mark.parametrize(
    "reply",
    [
        pytest.param(ConnectionRefusedError("refused"), id="unreachable"),
        pytest.param(b"not json\n", id="undecodable"),
        pytest.param(b'{"schema_version":"0.1","status":"error"}\n', id="not-ok"),
    ],
)
def test_an_unusable_agent_reply_still_falls_back(
    monkeypatch: pytest.MonkeyPatch, reply: bytes | Exception
) -> None:
    # Control: discovery stays best-effort.
    _agent_replying(monkeypatch, reply)
    assert resolve_serving_url(None, discover=True).source == "loopback"


def test_overrides_and_discover_false_never_ask_the_agent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Control: a resident set cannot refuse a caller that names the URL or
    # opts out of discovery.
    agent = _agent_replying(monkeypatch, _TWO_MEMBERS)
    assert resolve_serving_url("http://127.0.0.1:18091", discover=True).source == "explicit"
    config = {
        "schema_version": "0.1",
        "default_profile": "local",
        "profiles": {
            "local": {
                "mode": "local",
                "socket_path": "/nonexistent",
                "serving_url": "http://127.0.0.1:18092",
            }
        },
    }
    config_file = tmp_path / "cli.json"
    config_file.write_text(json.dumps(config), encoding="utf-8")
    assert resolve_serving_url(None, config_path=str(config_file)).source == "profile"
    assert resolve_serving_url(None, discover=False).source == "loopback"
    assert agent.calls == 0


def test_client_construction_surfaces_endpoint_unavailable(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    _agent_replying(monkeypatch, _TWO_MEMBERS)
    with pytest.raises(EndpointUnavailableError):
        ServingClient()
    with pytest.raises(EndpointUnavailableError):
        VisionClient()
    assert "EndpointUnavailableError" in tensorplate.__all__
    assert tensorplate.EndpointUnavailableError is EndpointUnavailableError

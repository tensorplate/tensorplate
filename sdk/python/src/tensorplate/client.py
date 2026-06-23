"""Serving endpoint resolution and the low-level HTTP transport.

Resolution mirrors ``tensorplate infer`` so the SDK reaches the same
worker the CLI would: an explicit URL wins, then the active CLI profile's
``serving_url``, then a read-only agent-status discovery, then the
loopback default. URL canonicalization matches the CLI's exactly. The
HTTP transport is hand-rolled over the standard library so the core SDK
has no third-party dependency.
"""

from __future__ import annotations

import json
import os
import socket
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

from tensorplate.errors import (
    EndpointResolutionError,
    RequestTimeoutError,
    TransportError,
)

#: Loopback serving endpoint used when nothing else resolves. Mirrors the
#: CLI's hard-coded v0.1 default.
LOOPBACK_DEFAULT = "http://127.0.0.1:18080"

#: Default local agent control socket. Mirrors the CLI/packaging default.
DEFAULT_AGENT_SOCKET = "/var/run/tensorplate/agent.sock"

#: Environment variable naming the CLI config file. Mirrors the CLI's
#: discovery: explicit path, then this variable, then built-in defaults —
#: no arbitrary filesystem search.
CLI_CONFIG_ENV = "TENSORPLATE_CLI_CONFIG"

#: Default request timeout in seconds.
DEFAULT_TIMEOUT_S = 30.0


@dataclass(frozen=True)
class ResolvedEndpoint:
    """A canonicalized serving endpoint and how it was resolved."""

    url: str
    host: str
    port: int
    path: str
    source: str

    @property
    def origin(self) -> str:
        """The ``http://host:port`` origin, without the inference path."""
        return f"http://{self.host}:{self.port}"


def canonicalize_serving_url(value: str, source: str) -> ResolvedEndpoint:
    """Canonicalize a serving URL exactly as ``tensorplate infer`` does.

    A bare ``http://host:port`` gets the ``/infer`` path appended; a full
    URL with a path keeps that path. Only ``http://`` is accepted (v0.1
    serving is loopback HTTP).
    """
    if not value.startswith("http://"):
        raise EndpointResolutionError(
            f"serving url {value!r} must start with 'http://' (v0.1 serving is loopback http)"
        )
    rest = value[len("http://") :]
    authority, separator, tail = rest.partition("/")
    raw_path = (tail or "infer") if separator else "infer"
    if ":" in authority:
        host, _, port_text = authority.rpartition(":")
        try:
            port = int(port_text)
        except ValueError:
            raise EndpointResolutionError(f"serving url {value!r} has a non-numeric port") from None
    else:
        host, port = authority, 80
    if not host:
        raise EndpointResolutionError(f"serving url {value!r} has an empty host")
    if not 0 <= port <= 65535:
        raise EndpointResolutionError(f"serving url {value!r} has an out-of-range port: {port}")
    path = raw_path if raw_path.startswith("/") else f"/{raw_path}"
    return ResolvedEndpoint(
        url=f"http://{host}:{port}{path}", host=host, port=port, path=path, source=source
    )


def http_request(
    method: str,
    url: str,
    *,
    body: bytes | None = None,
    headers: dict[str, str] | None = None,
    timeout: float = DEFAULT_TIMEOUT_S,
) -> tuple[int, bytes]:
    """Perform a single HTTP request, returning ``(status_code, body)``.

    Non-2xx responses are returned (not raised) so callers can read a
    typed failure envelope or a ``/health`` degraded body. Only genuine
    transport failures raise :class:`TransportError` /
    :class:`RequestTimeoutError`.
    """
    request = urllib.request.Request(url, data=body, method=method)
    normalized_headers = {key.lower(): value for key, value in (headers or {}).items()}
    request.add_header("Accept", normalized_headers.get("accept", "application/json"))
    if body is not None and "content-type" not in normalized_headers:
        request.add_header("Content-Type", "application/json")
    for key, value in (headers or {}).items():
        request.add_header(key, value)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            data: bytes = response.read()
            return int(response.status), data
    except urllib.error.HTTPError as exc:
        return int(exc.code), exc.read()
    except TimeoutError as exc:
        raise RequestTimeoutError(f"request to {url!r} timed out after {timeout}s") from exc
    except urllib.error.URLError as exc:
        if isinstance(exc.reason, TimeoutError):
            raise RequestTimeoutError(f"request to {url!r} timed out after {timeout}s") from exc
        raise TransportError(f"failed to reach {url!r}: {exc.reason}") from exc
    except OSError as exc:
        raise TransportError(f"failed to reach {url!r}: {exc}") from exc


@dataclass(frozen=True)
class _AgentTransport:
    kind: str  # "unix" | "tcp"
    socket_path: str | None = None
    host: str | None = None
    port: int | None = None


def resolve_serving_url(
    explicit: str | None = None,
    *,
    profile: str | None = None,
    config_path: str | None = None,
    timeout: float = DEFAULT_TIMEOUT_S,
    discover: bool = True,
) -> ResolvedEndpoint:
    """Resolve the serving endpoint with CLI-parity precedence.

    Order: explicit URL, then the chosen CLI profile's ``serving_url``,
    then read-only agent-status discovery, then the loopback default. When
    ``discover`` is false the agent tier is skipped. Discovery is
    best-effort: an unreachable agent falls through to the loopback
    default rather than raising.
    """
    if explicit is not None:
        return canonicalize_serving_url(explicit, "explicit")
    config = _load_cli_config(config_path)
    serving_url, transport = _select_profile(config, profile)
    if serving_url is not None:
        return canonicalize_serving_url(serving_url, "profile")
    if discover and transport is not None:
        discovered = _discover_via_agent(transport, timeout)
        if discovered is not None:
            return canonicalize_serving_url(discovered, "agent-discovered")
    return canonicalize_serving_url(LOOPBACK_DEFAULT, "loopback")


def _load_cli_config(config_path: str | None) -> dict[str, object]:
    path: Path | None = None
    if config_path is not None:
        path = Path(config_path)
    else:
        env_path = os.environ.get(CLI_CONFIG_ENV)
        if env_path:
            path = Path(env_path)
    if path is None:
        return {}
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        raise EndpointResolutionError(f"failed to read CLI config {str(path)!r}: {exc}") from exc
    try:
        parsed = json.loads(text)
    except ValueError as exc:
        raise EndpointResolutionError(f"CLI config {str(path)!r} is not valid JSON: {exc}") from exc
    if not isinstance(parsed, dict):
        raise EndpointResolutionError(f"CLI config {str(path)!r} must be a JSON object")
    return parsed


def _select_profile(
    config: dict[str, object], profile: str | None
) -> tuple[str | None, _AgentTransport | None]:
    profiles_obj = config.get("profiles")
    profiles = profiles_obj if isinstance(profiles_obj, dict) else {}
    default_obj = config.get("default_profile")
    default_name = default_obj if isinstance(default_obj, str) else None
    name = profile or default_name or "local"
    spec = profiles.get(name)
    if not isinstance(spec, dict):
        if name == "local":
            return None, _AgentTransport(kind="unix", socket_path=DEFAULT_AGENT_SOCKET)
        raise EndpointResolutionError(f"profile {name!r} is not declared in the CLI config")
    serving_obj = spec.get("serving_url")
    serving_url = serving_obj if isinstance(serving_obj, str) else None
    return serving_url, _transport_for_spec(spec)


def _transport_for_spec(spec: dict[str, object]) -> _AgentTransport | None:
    mode = spec.get("mode")
    if mode == "url":
        agent_url = spec.get("agent_url")
        if not isinstance(agent_url, str) or ":" not in agent_url:
            return None
        host, _, port_text = agent_url.rpartition(":")
        try:
            port = int(port_text)
        except ValueError:
            return None
        if not host:
            return None
        return _AgentTransport(kind="tcp", host=host, port=port)
    if mode in (None, "local"):
        socket_obj = spec.get("socket_path")
        socket_path = socket_obj if isinstance(socket_obj, str) else DEFAULT_AGENT_SOCKET
        return _AgentTransport(kind="unix", socket_path=socket_path)
    # Reserved modes (ssh_tunnel/overlay/relay): the SDK does not attempt
    # discovery against them, mirroring the CLI's "unsupported" stance.
    return None


def _discover_via_agent(transport: _AgentTransport, timeout: float) -> str | None:
    """Query the agent's read-only ``status`` op for the active serving URL.

    Best-effort: any transport, decode, or shape problem returns ``None``
    so resolution falls through to the loopback default.
    """
    try:
        raw = _agent_status_roundtrip(transport, timeout)
    except OSError:
        return None
    try:
        parsed = json.loads(raw)
    except (ValueError, UnicodeDecodeError):
        return None
    if not isinstance(parsed, dict) or parsed.get("status") != "ok":
        return None
    agent_status = parsed.get("agent_status")
    if not isinstance(agent_status, dict):
        return None
    active = agent_status.get("active")
    if not isinstance(active, dict):
        return None
    serving_url = active.get("serving_url")
    return serving_url if isinstance(serving_url, str) and serving_url else None


def _agent_status_roundtrip(transport: _AgentTransport, timeout: float) -> bytes:
    request = json.dumps({"schema_version": "0.1", "op": "status"}).encode("utf-8") + b"\n"
    if transport.kind == "unix":
        path = transport.socket_path
        if path is None or not hasattr(socket, "AF_UNIX"):
            raise OSError("unix-socket agent transport is unavailable")
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(timeout)
        sock.connect(path)
    else:
        if transport.host is None or transport.port is None:
            raise OSError("tcp agent transport is missing host/port")
        sock = socket.create_connection((transport.host, transport.port), timeout=timeout)
    chunks: list[bytes] = []
    with sock:
        sock.sendall(request)
        while True:
            chunk = sock.recv(65536)
            if not chunk:
                break
            chunks.append(chunk)
            if b"\n" in chunk:
                break
    return b"".join(chunks)

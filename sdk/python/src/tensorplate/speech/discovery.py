"""Where the active deployment serves stream sessions, as the agent's status names it."""

from __future__ import annotations

import asyncio
import json
import re
from collections.abc import Callable
from typing import Any, NamedTuple

from tensorplate.client import (
    _agent_status_roundtrip,
    _AgentTransport,
    _load_cli_config,
    _select_profile,
)
from tensorplate.errors import EndpointResolutionError
from tensorplate.speech.errors import (
    DiscoveryError,
    NotServingError,
    NotStreamingError,
    RemoteAgentError,
    ResidentSetError,
)
from tensorplate.speech.events import Target

#: Seconds each socket operation of a status read may take: the connection, the request's
#: write and each wait for more of the answer.
DEFAULT_STATUS_TIMEOUT = 5.0

# The agent lists it when it can start a deployment's workers with their stream listener.
_STREAMING_FEATURE = "deploy_streaming"
_ENDPOINT = re.compile(r"(127\.0\.0\.1|localhost|\[::1\]):[0-9]+")
_DIGEST = re.compile(r"sha256:[0-9a-f]{64}")
_DEPLOYMENT = re.compile(r"[A-Za-z0-9._-]{1,128}")
_LOOPBACK_HOSTS = ("127.0.0.1", "localhost", "::1", "[::1]")
_PAGE = "see docs/sdk/python.md, 'Finding the stream endpoint'"


class StreamTarget(NamedTuple):
    """The stream endpoint of the active deployment and the target an Open names there."""

    endpoint: str
    target: Target


async def resolve_stream_target(
    *,
    profile: str | None = None,
    config_path: str | None = None,
    timeout: float = DEFAULT_STATUS_TIMEOUT,
) -> StreamTarget:
    """Read the agent's status once and return where the active deployment serves streams.

    The agent is found as ``tensorplate`` finds it: the CLI profile named by
    `profile`, else the config's default profile, else ``local``, from the
    config file at `config_path` or ``TENSORPLATE_CLI_CONFIG``; ``local``
    is the agent's socket at ``/var/run/tensorplate/agent.sock``. There is
    no fallback: when the status names no stream target, this raises a
    ``DiscoveryError`` saying why, and never guesses an endpoint.

    Raises ``NotStreamingError`` when the active deployment's serving
    generation does not serve streams, ``NotServingError`` when no
    generation serves it now (a deploy, rollback or restart is between its
    steps, or nothing is deployed), ``ResidentSetError`` for an agent
    serving more than one deployment, ``RemoteAgentError`` for an agent
    reached at a host other than this one, and ``DiscoveryError`` for an
    agent that cannot be reached or read. Nothing is retried, and nothing
    waits for a deployment to start serving: a caller that wants to wait
    calls this again.
    """
    transport = _transport(profile, config_path)
    return await asyncio.to_thread(_read, transport, timeout)


def _transport(profile: str | None, config_path: str | None) -> _AgentTransport:
    try:
        _, transport = _select_profile(_load_cli_config(config_path), profile)
    except EndpointResolutionError as error:
        raise DiscoveryError(str(error), admitted=False) from error
    if transport is None:
        raise DiscoveryError(
            f"the CLI profile names no agent the SDK reads a status from; {_PAGE}",
            admitted=False,
        )
    if transport.kind == "tcp" and transport.host not in _LOOPBACK_HOSTS:
        raise RemoteAgentError(
            f"the agent at {transport.host}:{transport.port} reports stream endpoints on its "
            f"own loopback interface, which this host cannot reach; give the client the "
            f"endpoint forwarded to it and the target; {_PAGE}",
            admitted=False,
        )
    return transport


def _read(transport: _AgentTransport, timeout: float) -> StreamTarget:
    try:
        raw = _agent_status_roundtrip(transport, timeout)
    except OSError as error:
        raise DiscoveryError(f"cannot read the agent's status: {error}", admitted=False) from None
    try:
        reply = json.loads(raw)
    except (ValueError, UnicodeDecodeError):
        reply = None
    status = reply.get("agent_status") if isinstance(reply, dict) else None
    if not isinstance(reply, dict) or reply.get("status") != "ok" or not isinstance(status, dict):
        raise DiscoveryError("the agent's status answer is not an ok status", admitted=False)
    return _stream_target(status)


def _stream_target(status: dict[str, Any]) -> StreamTarget:
    resident_set = status.get("resident_set")
    members = resident_set.get("members") if isinstance(resident_set, dict) else None
    if isinstance(members, list) and len(members) > 1:
        raise ResidentSetError(
            f"the agent serves {len(members)} deployments and no single active one; give "
            f"the client the member's stream endpoint and target; {_PAGE}",
            admitted=False,
        )
    features = status.get("control_features")
    if not isinstance(features, list) or _STREAMING_FEATURE not in features:
        raise NotStreamingError(
            f"the agent starts no deployment with streaming; {_PAGE}", admitted=False
        )
    active = status.get("active")
    if not isinstance(active, dict):
        raise NotServingError(f"no deployment is active; {_PAGE}", admitted=False)
    if "generation" not in active:
        raise NotServingError(
            f"no generation serves the active deployment now; {_PAGE}", admitted=False
        )
    if "stream_endpoint" not in active:
        raise NotStreamingError(
            f"the generation serving the active deployment does not serve streams; {_PAGE}",
            admitted=False,
        )
    endpoint = active.get("stream_endpoint")
    deployment_id = active.get("deployment_id")
    generation = active.get("generation")
    digest = active.get("descriptor_digest")
    if not (
        isinstance(endpoint, str)
        and _ENDPOINT.fullmatch(endpoint)
        and isinstance(deployment_id, str)
        and _DEPLOYMENT.fullmatch(deployment_id)
        and deployment_id not in (".", "..")
        and type(generation) is int
        and generation > 0
        and isinstance(digest, str)
        and _DIGEST.fullmatch(digest)
    ):
        raise DiscoveryError("the agent's status names a malformed stream target", admitted=False)
    return StreamTarget(endpoint, Target(deployment_id, generation, digest))


class AgentTargets:
    """The stream target a client resolved from the agent, read again when it may be stale."""

    def __init__(
        self,
        first: StreamTarget,
        connector: Callable[[StreamTarget], Callable[[], Any]],
        *,
        profile: str | None,
        config_path: str | None,
        timeout: float,
    ) -> None:
        self._current, self._stale = first, False
        # Counts stale() calls, so a read that started before one does not clear it.
        self._marks = 0
        self.connector = connector
        self._profile, self._config_path = profile, config_path
        self._timeout = timeout
        self._reading = asyncio.Lock()

    @property
    def current(self) -> StreamTarget:
        return self._current

    async def fresh(self) -> StreamTarget:
        """The target to open on: the last one read, unless a session's end made it stale."""
        if self._stale:
            return await self._read(None)
        return self._current

    async def after(self, seen: StreamTarget) -> StreamTarget:
        """The target as the agent names it now, after an Open on `seen` was lost.

        When another session read a target other than `seen` meanwhile, that
        one is returned without a second read.
        """
        return await self._read(seen)

    def stale(self) -> None:
        """The worker that served the target ended its sessions: read before the next Open."""
        self._stale = True
        self._marks += 1

    async def _read(self, seen: StreamTarget | None) -> StreamTarget:
        async with self._reading:
            if seen is not None and self._current != seen and not self._stale:
                return self._current
            if seen is None and not self._stale:
                return self._current
            marks = self._marks
            transport = _transport(self._profile, self._config_path)
            self._current = await asyncio.to_thread(_read, transport, self._timeout)
            if self._marks == marks:
                self._stale = False
            return self._current

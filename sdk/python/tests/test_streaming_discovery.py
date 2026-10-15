"""The stream target as the agent's status names it, and a client that follows it.

The agent here answers its control socket's ``status`` op with documents the
agent recorded over that socket (``protocol/rust/tests/fixtures/``). A case
that points one at an in-process server replaces the recorded endpoint, and
for another generation its generation and digest, and nothing else.
"""

from __future__ import annotations

import asyncio
import contextlib
import copy
import gc
import json
import os
import shutil
import tempfile
from collections.abc import AsyncIterator, Awaitable, Callable
from dataclasses import dataclass, field
from typing import Any

import pytest

from tensorplate.errors import EndpointResolutionError, MissingDependencyError

try:
    import tensorplate.speech as speech
except MissingDependencyError as missing:
    if os.environ.get("TENSORPLATE_REQUIRE_SPEECH_EXTRAS") == "1":
        raise
    pytest.skip(str(missing), allow_module_level=True)

import grpc.aio

from stream_server import REPOSITORY, ResettingRelay, StreamServer

FIXTURES = REPOSITORY / "protocol/rust/tests/fixtures"
PCM = speech.AudioFormat("pcm_s16le", 16000)
FRAME = bytes(640)
RECORDED_DIGEST = "sha256:5408fc199178892bf22cbe2354a6160d7e4d2e8de41a92611488e0ecdc72ee3a"
TARGET = speech.Target("stt", 1, RECORDED_DIGEST)
# The recorded endpoint and target; a StreamTarget equals the plain pair.
RECORDED = ("127.0.0.1:41285", TARGET)
# The digest of a later generation's descriptor.
LATER_DIGEST = "sha256:" + "1" * 64


def reply(file: str, line: int) -> dict[str, Any]:
    """The reply on line `line` (from 1) of a recorded control exchange."""
    text = (FIXTURES / f"agent_control_{file}.jsonl").read_text("utf-8").splitlines()[line - 1]
    document: dict[str, Any] = json.loads(text)
    assert "agent_status" in document
    return document


def streaming(endpoint: str, generation: int = 1, digest: str = RECORDED_DIGEST) -> dict[str, Any]:
    """The recorded status after a deploy with streaming, its stream target moved to `endpoint`."""
    document = copy.deepcopy(reply("streaming_deploy", 4))
    active = document["agent_status"]["active"]
    active.update(stream_endpoint=endpoint, generation=generation, descriptor_digest=digest)
    return document


def server(generation: int = 1, digest: str = RECORDED_DIGEST, **options: Any) -> StreamServer:
    """A worker of the recorded deployment, serving `generation`."""
    return StreamServer(
        deployment_id="stt", generation=generation, descriptor_digest=digest, **options
    )


def run(main: Callable[[], Awaitable[Any]]) -> Any:
    """Run `main` in a loop of its own; nothing may be left for asyncio to report."""
    reported: list[dict[str, Any]] = []

    async def watched() -> Any:
        asyncio.get_running_loop().set_exception_handler(
            lambda _, context: reported.append(context)
        )
        result = await main()
        gc.collect()
        await asyncio.sleep(0)
        return result

    result = asyncio.run(asyncio.wait_for(watched(), 60))
    assert reported == []
    return result


@dataclass
class Agent:
    """An agent's control socket that answers ``status`` with `answer`, or closes on ``None``."""

    answer: dict[str, Any] | bytes | None
    reads: int = 0
    # A read is answered once this is set; `asked` is set when one arrives.
    hold: asyncio.Event | None = None
    asked: asyncio.Event = field(default_factory=asyncio.Event)

    async def _serve(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        request = json.loads(await reader.readline())
        assert request == {"schema_version": "0.1", "op": "status"}
        self.reads += 1
        self.asked.set()
        if self.hold is not None:
            await self.hold.wait()
        answer = self.answer
        if answer is not None:
            data = answer if isinstance(answer, bytes) else json.dumps(answer).encode()
            writer.write(data.rstrip(b"\n") + b"\n")
            await writer.drain()
        writer.close()

    @contextlib.asynccontextmanager
    async def serving(self, tcp: bool = False) -> AsyncIterator[str]:
        """Serve, and give the path of a CLI config whose default profile reaches this agent."""
        # Short, as socket paths are bounded and a test's own directory may be too long.
        directory = tempfile.mkdtemp(prefix="tp-agent", dir="/tmp")
        path = os.path.join(directory, "a.sock")
        if tcp:
            listener = await asyncio.start_server(self._serve, "127.0.0.1", 0)
            port = listener.sockets[0].getsockname()[1]
            profile: dict[str, Any] = {"mode": "url", "agent_url": f"127.0.0.1:{port}"}
        else:
            listener = await asyncio.start_unix_server(self._serve, path)
            profile = {"mode": "local", "socket_path": path}
        config = os.path.join(directory, "cli.json")
        with open(config, "w", encoding="utf-8") as file:
            json.dump({"schema_version": "0.1", "profiles": {"local": profile}}, file)
        try:
            yield config
        finally:
            if self.hold is not None:
                self.hold.set()
            listener.close()
            await listener.wait_closed()
            shutil.rmtree(directory, ignore_errors=True)


def resolved(answer: dict[str, Any] | bytes | None, tcp: bool = False) -> speech.StreamTarget:
    async def main() -> speech.StreamTarget:
        async with Agent(answer).serving(tcp) as config:
            return await speech.resolve_stream_target(config_path=config)

    result: speech.StreamTarget = run(main)
    return result


def refused(answer: dict[str, Any] | bytes | None) -> speech.DiscoveryError:
    with pytest.raises(speech.DiscoveryError) as caught:
        resolved(answer)
    error = caught.value
    assert isinstance(error, EndpointResolutionError)
    assert (error.reason, error.code, error.grpc_status, error.admitted) == (
        None,
        None,
        None,
        False,
    )
    return error


# --- reading the status ------------------------------------------------------


@pytest.mark.parametrize("line", [2, 4])
def test_a_recorded_streaming_status_names_its_endpoint_and_target(line: int) -> None:
    raw = (FIXTURES / "agent_control_streaming_deploy.jsonl").read_bytes().splitlines()[line - 1]
    found = resolved(raw)
    assert (found.endpoint, found.target) == RECORDED
    endpoint, target = resolved(raw)
    assert (endpoint, target) == RECORDED


def test_an_agent_on_loopback_tcp_is_read() -> None:
    assert resolved(reply("streaming_deploy", 2), tcp=True) == RECORDED


@pytest.mark.parametrize(
    ("file", "line"),
    [
        ("singleton_lifecycle", 2),
        ("singleton_lifecycle", 4),
        ("singleton_lifecycle", 6),
        ("singleton_lifecycle", 8),
        ("member_status", 2),
        ("member_status", 4),
    ],
)
def test_an_agent_that_starts_no_deployment_with_streaming_names_no_stream(
    file: str, line: int
) -> None:
    assert type(refused(reply(file, line))) is speech.NotStreamingError


def test_an_agent_serving_more_than_one_deployment_names_no_single_stream() -> None:
    # A set of one, as recorded, beside the streaming status's `active`.
    document = streaming("127.0.0.1:41285")
    members = reply("member_status", 4)["agent_status"]["resident_set"]
    document["agent_status"]["resident_set"] = members
    assert resolved(document) == RECORDED
    # With a second member the agent reports no `active`.
    members["members"].append(dict(members["members"][0], deployment_id="stt-two"))
    del document["agent_status"]["active"]
    assert type(refused(document)) is speech.ResidentSetError


def test_an_agent_at_another_host_is_not_asked(monkeypatch: pytest.MonkeyPatch) -> None:
    def asked(*arguments: Any) -> bytes:
        raise AssertionError("the agent was asked")

    # The module the function reads it from, whichever copy of the package was imported.
    monkeypatch.setitem(speech.resolve_stream_target.__globals__, "_agent_status_roundtrip", asked)
    directory = tempfile.mkdtemp(prefix="tp-cli")
    try:
        config = os.path.join(directory, "cli.json")
        profiles = {
            "remote": {"mode": "url", "agent_url": "192.0.2.10:7070"},
            "tunnel": {"mode": "ssh_tunnel"},
        }
        with open(config, "w", encoding="utf-8") as file:
            json.dump({"schema_version": "0.1", "profiles": profiles}, file)

        async def main(profile: str) -> Any:
            return await speech.resolve_stream_target(profile=profile, config_path=config)

        for profile, error in [
            ("remote", speech.RemoteAgentError),
            ("tunnel", speech.DiscoveryError),
            ("undeclared", speech.DiscoveryError),
        ]:
            with pytest.raises(speech.DiscoveryError) as caught:
                run(lambda profile=profile: main(profile))  # type: ignore[misc]
            assert type(caught.value) is error
            assert caught.value.admitted is False
    finally:
        shutil.rmtree(directory, ignore_errors=True)


def test_an_agent_whose_socket_is_absent_names_no_stream(tmp_path: Any) -> None:
    config = tmp_path / "cli.json"
    profile = {"mode": "local", "socket_path": str(tmp_path / "absent.sock")}
    config.write_text(json.dumps({"schema_version": "0.1", "profiles": {"local": profile}}))

    async def main() -> Any:
        return await speech.resolve_stream_target(config_path=str(config))

    with pytest.raises(speech.DiscoveryError) as caught:
        run(main)
    assert type(caught.value) is speech.DiscoveryError
    assert "cannot read the agent's status" in str(caught.value)


def test_an_agent_that_cannot_be_read_names_no_stream() -> None:
    for answer in [None, b"not json", {"schema_version": "0.1", "status": "error"}]:
        assert type(refused(answer)) is speech.DiscoveryError
    ok = reply("streaming_deploy", 2)
    ok["agent_status"] = []
    assert type(refused(ok)) is speech.DiscoveryError
    # An answer that is not ok names no target, whatever else it carries.
    failed = reply("streaming_deploy", 2)
    failed["status"] = "error"
    assert type(refused(failed)) is speech.DiscoveryError


@pytest.mark.parametrize(
    ("key", "value"),
    [
        ("stream_endpoint", "192.0.2.1:41285"),
        ("stream_endpoint", "127.0.0.1"),
        ("stream_endpoint", 41285),
        ("generation", 0),
        ("generation", True),
        ("generation", "1"),
        ("descriptor_digest", RECORDED_DIGEST.upper()),
        ("descriptor_digest", None),
        ("deployment_id", ".."),
        ("deployment_id", "a b"),
    ],
)
def test_a_malformed_stream_target_is_refused(key: str, value: Any) -> None:
    document = reply("streaming_deploy", 2)
    document["agent_status"]["active"][key] = value
    assert type(refused(document)) is speech.DiscoveryError


# --- a client made from the status -----------------------------------------


Scenario = Callable[..., Awaitable[Any]]


def following(scenario: Scenario, *servers: StreamServer) -> Any:
    """Serve `servers`; the agent names the first; run `scenario` with a client from the agent."""

    async def main() -> Any:
        async with contextlib.AsyncExitStack() as stack:
            endpoints = [await stack.enter_async_context(fake.serving()) for fake in servers]
            agent = Agent(streaming(endpoints[0]))
            config = await stack.enter_async_context(agent.serving())
            stream = await speech.SpeechClient.from_agent(config_path=config)
            async with stream:
                return await scenario(agent, stream, *endpoints)

    return run(main)


async def converse(stream: speech.SpeechClient) -> speech.SessionTotals:
    async with stream.transcribe(language="en", input_format=PCM) as session:
        await session.send_audio(FRAME)
        totals = await session.close()
    return totals


def opened(fake: StreamServer) -> list[tuple[str, int, str]]:
    """The target each Open the server read names."""
    return [
        (
            event.open.resolved.deployment_id,
            event.open.resolved.generation,
            event.open.resolved.descriptor_digest,
        )
        for event in fake.received
        if event.WhichOneof("body") == "open"
    ]


def test_a_client_from_the_agent_opens_on_the_target_it_names() -> None:
    fake = server()

    async def scenario(agent: Agent, stream: speech.SpeechClient, endpoint: str) -> None:
        for _ in range(2):
            totals = await converse(stream)
            assert totals.input_audio_samples == len(FRAME) // 2
        async with stream.synthesize(language="en", voice="synthetic_voice"):
            pass
        assert opened(fake) == [("stt", 1, RECORDED_DIGEST)] * 3
        assert agent.reads == 1

    following(scenario, fake)


def test_a_target_is_given_to_a_client_with_an_endpoint_and_never_to_one_from_the_agent() -> None:
    async def scenario(agent: Agent, stream: speech.SpeechClient, endpoint: str) -> None:
        with pytest.raises(ValueError, match="from_agent"):
            stream.transcribe(TARGET, language="en", input_format=PCM)
        with pytest.raises(ValueError, match="from_agent"):
            stream.synthesize(TARGET, language="en", voice="synthetic_voice")
        async with speech.SpeechClient(endpoint) as given:
            with pytest.raises(ValueError, match="target"):
                given.transcribe(language="en", input_format=PCM)
            with pytest.raises(ValueError, match="target"):
                given.synthesize(language="en", voice="synthetic_voice")

    following(scenario, server())


@pytest.mark.parametrize("outgoing", ["admission_closed", "gone"])
def test_an_open_refused_by_the_outgoing_worker_follows_the_agent_to_the_next_generation(
    outgoing: str,
) -> None:
    old, new = server(), server(generation=2, digest=LATER_DIGEST)

    async def main() -> None:
        async with new.serving() as next_endpoint, contextlib.AsyncExitStack() as stack:
            endpoint = await stack.enter_async_context(old.serving())
            agent = Agent(streaming(endpoint))
            async with agent.serving() as config:
                stream = await speech.SpeechClient.from_agent(config_path=config)
                async with stream:
                    await converse(stream)
                    agent.answer = streaming(next_endpoint, 2, LATER_DIGEST)
                    if outgoing == "gone":
                        await stack.aclose()
                    else:
                        old.admission_open = False
                    await converse(stream)
                    assert agent.reads == 2
        assert opened(new) == [("stt", 2, LATER_DIGEST)]
        assert old.calls == (1 if outgoing == "gone" else 2)

    run(main)


def test_an_open_refused_as_stale_after_a_restart_follows_the_new_generation_on_its_port() -> None:
    fake = server()

    async def scenario(agent: Agent, stream: speech.SpeechClient, endpoint: str) -> None:
        await converse(stream)
        fake.generation, fake.descriptor_digest = 2, LATER_DIGEST
        agent.answer = streaming(endpoint, 2, LATER_DIGEST)
        await converse(stream)
        assert [refusal.cause.reason for refusal in fake.refusals] == [
            speech_reason("stale_generation")
        ]
        assert opened(fake)[1:] == [("stt", 1, RECORDED_DIGEST), ("stt", 2, LATER_DIGEST)]
        assert (agent.reads, fake.calls) == (2, 3)
        assert list(stream._stubs) == [endpoint]
        assert len(stream._channels) == 1

    following(scenario, fake)


def speech_reason(name: str) -> int:
    from tensorplate.stream.v1 import session_pb2 as pb

    return int(pb.FailureReason.Value(f"FAILURE_REASON_{name.upper()}"))


def test_an_open_refused_on_a_target_the_agent_still_names_is_not_sent_again() -> None:
    fake = server()

    async def scenario(agent: Agent, stream: speech.SpeechClient, endpoint: str) -> None:
        session = stream.transcribe(language="fr", input_format=PCM)
        with pytest.raises(speech.PreconditionFailedError) as caught:
            await session.open()
        assert caught.value.admitted is None
        assert caught.value.__context__ is None
        assert (agent.reads, fake.calls) == (2, 1)

    following(scenario, fake)


def relayed(scenario: Callable[..., Awaitable[Any]], *servers: StreamServer) -> Any:
    """As `following`, with the agent naming a relay to the first server."""

    async def main() -> Any:
        async with contextlib.AsyncExitStack() as stack:
            endpoints = [await stack.enter_async_context(fake.serving()) for fake in servers]
            relay = ResettingRelay(endpoints[0])
            through = await stack.enter_async_context(relay.serving())
            agent = Agent(streaming(through))
            config = await stack.enter_async_context(agent.serving())
            async with await speech.SpeechClient.from_agent(config_path=config) as stream:
                await converse(stream)
                return await scenario(agent, stream, relay, *endpoints)

    return run(main)


def test_an_open_lost_on_a_target_the_agent_still_names_is_sent_once_more_there() -> None:
    fake = server()

    async def scenario(
        agent: Agent, stream: speech.SpeechClient, relay: ResettingRelay, endpoint: str
    ) -> None:
        relay.arm("data")
        await converse(stream)
        assert (relay.resets, relay.calls, agent.reads) == (1, 3, 2)

    relayed(scenario, fake)


@pytest.mark.parametrize("case", ["lost_then_moved", "stale_then_lost", "lost_twice"])
def test_an_open_makes_at_most_two_calls_whether_it_follows_the_agent_or_not(case: str) -> None:
    fake, other = server(), server(generation=2, digest=LATER_DIGEST, admission_open=False)

    async def scenario(
        agent: Agent,
        stream: speech.SpeechClient,
        relay: ResettingRelay,
        endpoint: str,
        other_endpoint: str,
    ) -> None:
        calls = relay.calls
        agent.hold, agent.asked = asyncio.Event(), asyncio.Event()
        if case == "lost_then_moved":
            relay.arm("data")
            agent.answer = streaming(other_endpoint, 2, LATER_DIGEST)
        elif case == "stale_then_lost":
            fake.generation, fake.descriptor_digest = 2, LATER_DIGEST
            through = stream._targets.current.endpoint  # type: ignore[union-attr]
            agent.answer = streaming(through, 2, LATER_DIGEST)
        else:
            relay.arm("data", times=3)
        session = stream.transcribe(language="en", input_format=PCM)
        opening = asyncio.create_task(session.open())
        await agent.asked.wait()
        if case == "stale_then_lost":
            # The first call was refused; its second is cut at its Open.
            relay.arm("data")
        agent.hold.set()
        with pytest.raises(speech.ServiceUnavailableError) as caught:
            await opening
        assert caught.value.__context__ is None
        assert agent.reads == 2
        made = relay.calls - calls + other.calls
        assert made == 2

    relayed(scenario, fake, other)


def test_an_open_whose_target_cannot_be_read_again_raises_why_with_its_call_as_cause() -> None:
    fake = server()

    async def scenario(agent: Agent, stream: speech.SpeechClient, endpoint: str) -> None:
        fake.admission_open = False
        agent.answer = {"schema_version": "0.1", "status": "error"}
        session = stream.transcribe(language="en", input_format=PCM)
        with pytest.raises(speech.DiscoveryError) as caught:
            await session.open()
        error = caught.value
        assert isinstance(error.__cause__, speech.ServiceUnavailableError)
        assert (error.admitted, error.__context__) == (None, None)
        assert (agent.reads, fake.calls) == (2, 1)
        assert session._ended
        with pytest.raises(speech.DiscoveryError):
            await session.__anext__()
        # The next open reads before its first call.
        fake.admission_open = True
        agent.answer = streaming(endpoint)
        await converse(stream)
        assert (agent.reads, fake.calls) == (3, 2)

    following(scenario, fake)


async def held_read(agent: Agent) -> asyncio.Event:
    """Hold the agent's next status read; the event it returns is set once it arrives."""
    agent.hold, agent.asked = asyncio.Event(), asyncio.Event()
    return agent.asked


@pytest.mark.parametrize("end", ["cancel", "aclose", "timeout"])
def test_an_open_ended_while_it_reads_its_target_again_makes_no_second_call(end: str) -> None:
    old, new = server(admission_open=False), server(generation=2, digest=LATER_DIGEST)

    async def scenario(
        agent: Agent, stream: speech.SpeechClient, endpoint: str, next_endpoint: str
    ) -> None:
        asked = await held_read(agent)
        agent.answer = streaming(next_endpoint, 2, LATER_DIGEST)
        session = stream.transcribe(language="en", input_format=PCM, open_timeout=0.5)
        opening = asyncio.create_task(session.open())
        await asked.wait()
        if end == "timeout":
            with pytest.raises(speech.SessionTimeoutError):
                await opening
        else:
            await (session.cancel() if end == "cancel" else stream.aclose())
            assert session._ended
            first = session._call
            assert agent.hold is not None
            agent.hold.set()
            with pytest.raises(speech.SessionClosedError):
                await opening
            # The read ended after the session did, and no second call was made on it.
            assert (session._call, session._reopened) == (first, False)
        assert agent.hold is not None
        agent.hold.set()
        await asyncio.sleep(0.05)
        assert (old.calls, new.calls) == (1, 0)

    following(scenario, old, new)


@pytest.mark.parametrize("reason", ["worker_shutdown", "deployment_retired"])
def test_a_session_whose_worker_went_away_has_the_next_open_read_the_target_first(
    reason: str,
) -> None:
    old, new = server(), server(generation=2, digest=LATER_DIGEST)

    async def scenario(
        agent: Agent, stream: speech.SpeechClient, endpoint: str, next_endpoint: str
    ) -> None:
        session = stream.transcribe(language="en", input_format=PCM)
        await session.open()
        old.end(reason)
        with pytest.raises(speech.ServiceUnavailableError) as caught:
            await session.close()
        assert (caught.value.reason, caught.value.admitted) == (reason, True)
        agent.answer = streaming(next_endpoint, 2, LATER_DIGEST)
        await converse(stream)
        assert (agent.reads, old.calls, new.calls) == (2, 1, 1)

    following(scenario, old, new)


def test_a_worker_that_goes_away_during_a_read_leaves_the_target_to_read_again() -> None:
    old, new = server(), server(generation=2, digest=LATER_DIGEST)

    async def scenario(
        agent: Agent, stream: speech.SpeechClient, endpoint: str, next_endpoint: str
    ) -> None:
        live = stream.transcribe(language="en", input_format=PCM)
        await live.open()
        old.admission_open = False
        asked = await held_read(agent)
        refused = asyncio.create_task(converse(stream))
        await asked.wait()
        # The read in flight began before the worker ended the live session.
        old.end("worker_shutdown")
        with pytest.raises(speech.ServiceUnavailableError):
            await live.close()
        assert agent.hold is not None
        agent.hold.set()
        with pytest.raises(speech.ServiceUnavailableError):
            await refused
        # The refused open read the old target and sent its second call there.
        assert (agent.reads, old.calls) == (2, 3)
        agent.hold, agent.answer = None, streaming(next_endpoint, 2, LATER_DIGEST)
        await converse(stream)
        assert (agent.reads, old.calls, new.calls) == (3, 3, 1)

    following(scenario, old, new)


def test_a_session_ended_for_another_reason_leaves_the_target_as_read() -> None:
    fake, other = server(), server(generation=2, digest=LATER_DIGEST)

    async def scenario(
        agent: Agent, stream: speech.SpeechClient, endpoint: str, other_endpoint: str
    ) -> None:
        session = stream.transcribe(language="en", input_format=PCM)
        await session.open()
        fake.end("backend_reset")
        with pytest.raises(speech.ServiceUnavailableError):
            await session.close()
        agent.answer = streaming(other_endpoint, 2, LATER_DIGEST)
        await converse(stream)
        assert (agent.reads, fake.calls, other.calls) == (1, 2, 0)

    following(scenario, fake, other)


@pytest.mark.parametrize("end", ["cancel", "aclose", "timeout", "caller_cancelled", "open_again"])
def test_an_open_ended_while_it_reads_a_stale_target_sends_no_open(end: str) -> None:
    old, new = server(), server(generation=2, digest=LATER_DIGEST)

    async def scenario(
        agent: Agent, stream: speech.SpeechClient, endpoint: str, next_endpoint: str
    ) -> None:
        session = stream.transcribe(language="en", input_format=PCM)
        await session.open()
        old.end("worker_shutdown")
        with pytest.raises(speech.ServiceUnavailableError):
            await session.close()
        asked = await held_read(agent)
        agent.answer = streaming(next_endpoint, 2, LATER_DIGEST)
        session = stream.transcribe(language="en", input_format=PCM, open_timeout=0.5)
        opening = asyncio.create_task(session.open())
        await asked.wait()
        assert agent.hold is not None
        if end == "open_again":
            with pytest.raises(speech.SessionClosedError):
                await session.open()
            agent.hold.set()
            await opening
            await session.cancel()
            assert (old.calls, new.calls) == (1, 1)
            return
        if end == "timeout":
            with pytest.raises(speech.SessionTimeoutError):
                await opening
        elif end == "caller_cancelled":
            opening.cancel()
            with pytest.raises(asyncio.CancelledError):
                await opening
        else:
            await (session.cancel() if end == "cancel" else stream.aclose())
            assert session._ended
        agent.hold.set()
        if end in ("cancel", "aclose"):
            with pytest.raises(speech.SessionClosedError):
                await opening
        # The session ended with no call made, and stays ended.
        assert session._ended
        assert session._call is None
        with pytest.raises(speech.SessionClosedError):
            await session.open()
        if end in ("cancel", "aclose"):
            assert [event async for event in session] == []
        else:
            with pytest.raises(speech.SpeechError):
                await session.__anext__()
        await asyncio.sleep(0.05)
        assert (old.calls, new.calls) == (1, 0)
        if end == "aclose":
            # The read that ended after the close opened no channel the close missed.
            for stub in stream._stubs.values():
                with pytest.raises(grpc.aio.UsageError):
                    stub.Session()
            return
        # A cancelled session's open finished its read, so the next open needs none; an open
        # that timed out or whose caller was cancelled gave its read up.
        await converse(stream)
        gave_up = end in ("timeout", "caller_cancelled")
        assert (agent.reads, new.calls) == (3 if gave_up else 2, 1)

    following(scenario, old, new)


def test_a_session_open_on_a_generation_stays_there_when_the_agent_names_another() -> None:
    old, new = server(), server(generation=2, digest=LATER_DIGEST)

    async def scenario(
        agent: Agent, stream: speech.SpeechClient, endpoint: str, next_endpoint: str
    ) -> None:
        async with stream.transcribe(language="en", input_format=PCM) as session:
            agent.answer = streaming(next_endpoint, 2, LATER_DIGEST)
            # Another session's open is refused and reads the new target meanwhile.
            old.max_sessions = 2
            old.admission_open = False
            await converse(stream)
            await session.send_audio(FRAME)
            totals = await session.close()
        assert totals.input_audio_samples == len(FRAME) // 2
        assert (old.calls, new.calls, agent.reads) == (2, 1, 2)

    following(scenario, old, new)


def test_opens_lost_together_read_the_target_once() -> None:
    old, new = (
        server(admission_open=False),
        server(generation=2, digest=LATER_DIGEST, max_sessions=2),
    )

    async def scenario(
        agent: Agent, stream: speech.SpeechClient, endpoint: str, next_endpoint: str
    ) -> None:
        asked = await held_read(agent)
        agent.answer = streaming(next_endpoint, 2, LATER_DIGEST)
        sessions = [stream.transcribe(language="en", input_format=PCM) for _ in range(2)]
        openings = [asyncio.create_task(session.open()) for session in sessions]
        await asked.wait()
        await old.until(lambda: len(old.refusals) == 2)
        assert agent.hold is not None
        agent.hold.set()
        await asyncio.gather(*openings)
        assert (agent.reads, old.calls, new.calls) == (2, 2, 2)
        for session in sessions:
            await session.cancel()

    following(scenario, old, new)


def test_closing_the_client_closes_every_channel_it_opened() -> None:
    old, new = server(admission_open=False), server(generation=2, digest=LATER_DIGEST)

    async def main() -> None:
        async with old.serving() as endpoint, new.serving() as next_endpoint:
            agent = Agent(streaming(endpoint))
            async with agent.serving() as config:
                stream = await speech.SpeechClient.from_agent(config_path=config)
                agent.answer = streaming(next_endpoint, 2, LATER_DIGEST)
                await converse(stream)
                stubs = dict(stream._stubs)
                assert sorted(stubs) == sorted([endpoint, next_endpoint])
                await stream.aclose()
        for stub in stubs.values():
            with pytest.raises(grpc.aio.UsageError):
                stub.Session()

    run(main)


def test_a_refused_open_carries_the_listeners_text() -> None:
    fake = server(generation=2)

    async def main() -> None:
        async with fake.serving() as endpoint, speech.SpeechClient(endpoint) as stream:
            call = stream._stub.Session()
            from tensorplate.stream.v1 import session_pb2 as pb

            opening = pb.Open(
                mode=pb.SESSION_MODE_STT_STREAMING,
                language="en",
                input_format=pb.AudioFormat(
                    encoding=pb.AUDIO_ENCODING_PCM_S16LE, sample_rate_hz=16000, channels=1
                ),
                resolved=pb.ResolvedTarget(
                    deployment_id="stt", generation=1, descriptor_digest=RECORDED_DIGEST
                ),
            )
            await call.write(pb.ClientEvent(sequence=1, open=opening))
            assert await call.code() == grpc.StatusCode.FAILED_PRECONDITION
            # As the stream listener words it (test/integration/stream_bounds_test.cpp).
            assert await call.details() == "open refused: FAILURE_REASON_STALE_GENERATION"

    run(main)


def test_opens_that_find_the_target_stale_together_read_it_once() -> None:
    old, new = server(max_sessions=2), server(generation=2, digest=LATER_DIGEST, max_sessions=2)

    async def scenario(
        agent: Agent, stream: speech.SpeechClient, endpoint: str, next_endpoint: str
    ) -> None:
        session = stream.transcribe(language="en", input_format=PCM)
        await session.open()
        old.end("worker_shutdown")
        with pytest.raises(speech.ServiceUnavailableError):
            await session.close()
        asked = await held_read(agent)
        agent.answer = streaming(next_endpoint, 2, LATER_DIGEST)
        sessions = [stream.transcribe(language="en", input_format=PCM) for _ in range(2)]
        openings = [asyncio.create_task(session.open()) for session in sessions]
        await asked.wait()
        # The second open waits for the first one's read.
        targets = stream._targets
        assert targets is not None
        while not targets._reading._waiters:
            await asyncio.sleep(0)
        assert agent.hold is not None
        agent.hold.set()
        await asyncio.gather(*openings)
        assert (agent.reads, old.calls, new.calls) == (2, 1, 2)
        for session in sessions:
            await session.cancel()

    following(scenario, old, new)


def test_an_open_whose_stale_target_cannot_be_read_ends_its_session() -> None:
    old = server()

    async def scenario(agent: Agent, stream: speech.SpeechClient, endpoint: str) -> None:
        session = stream.transcribe(language="en", input_format=PCM)
        await session.open()
        old.end("worker_shutdown")
        with pytest.raises(speech.ServiceUnavailableError):
            await session.close()
        agent.answer = {"schema_version": "0.1", "status": "error"}
        session = stream.transcribe(language="en", input_format=PCM)
        with pytest.raises(speech.DiscoveryError) as caught:
            await session.open()
        assert (caught.value.admitted, caught.value.__cause__) == (False, None)
        assert session._ended
        assert session._call is None
        with pytest.raises(speech.DiscoveryError):
            await session.__anext__()
        with pytest.raises(speech.SessionClosedError):
            await session.open()
        assert (agent.reads, old.calls) == (2, 1)

    following(scenario, old)


def test_an_open_lost_after_another_read_a_target_that_went_stale_reads_it_again(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    first = server(admission_open=False)
    second = server(generation=2, digest=LATER_DIGEST)
    third = server(generation=3, digest="sha256:" + "3" * 64)

    async def scenario(agent: Agent, stream: speech.SpeechClient, *endpoints: str) -> None:
        held = stream.transcribe(language="en", input_format=PCM)
        lost, resume = asyncio.Event(), asyncio.Event()
        follow = held._follow

        async def holding(failure: Any) -> None:
            lost.set()
            await resume.wait()
            await follow(failure)

        monkeypatch.setattr(held, "_follow", holding)
        opening = asyncio.create_task(held.open())
        await lost.wait()
        # Another session reads the second target, is served there and loses its worker.
        agent.answer = streaming(endpoints[1], 2, LATER_DIGEST)
        other = stream.transcribe(language="en", input_format=PCM)
        await other.open()
        second.end("worker_shutdown")
        with pytest.raises(speech.ServiceUnavailableError):
            await other.close()
        agent.answer = streaming(endpoints[2], 3, "sha256:" + "3" * 64)
        resume.set()
        await opening
        await held.cancel()
        assert (first.calls, second.calls, third.calls, agent.reads) == (2, 1, 1, 3)

    following(scenario, first, second, third)


def test_an_open_that_ends_ok_before_any_event_is_a_protocol_error_and_reads_nothing() -> None:
    fake = server(fault="ok_before_ready")

    async def scenario(agent: Agent, stream: speech.SpeechClient, endpoint: str) -> None:
        session = stream.transcribe(language="en", input_format=PCM)
        with pytest.raises(speech.StreamProtocolError):
            await session.open()
        assert (agent.reads, fake.calls) == (1, 1)

    following(scenario, fake)


@pytest.mark.parametrize(
    ("file", "error"),
    [
        ("no_deployment_status", speech.NotServingError),
        ("streaming_handover", speech.NotServingError),
        ("unary_status", speech.NotStreamingError),
    ],
)
def test_a_recorded_status_that_names_no_stream_target_says_why(file: str, error: type) -> None:
    assert type(refused(reply(file, 2))) is error


def test_an_open_refused_in_a_handover_window_says_so_and_the_next_open_finds_the_new_target() -> (
    None
):
    old, new = server(admission_open=False), server(generation=2, digest=LATER_DIGEST)

    async def scenario(
        agent: Agent, stream: speech.SpeechClient, endpoint: str, next_endpoint: str
    ) -> None:
        agent.answer = reply("streaming_handover", 2)
        session = stream.transcribe(language="en", input_format=PCM)
        with pytest.raises(speech.NotServingError) as caught:
            await session.open()
        assert isinstance(caught.value.__cause__, speech.ServiceUnavailableError)
        assert caught.value.admitted is None
        # The caller opens again once the new generation serves; that open reads first.
        agent.answer = streaming(next_endpoint, 2, LATER_DIGEST)
        await converse(stream)
        assert (agent.reads, old.calls, new.calls) == (3, 1, 1)

    following(scenario, old, new)

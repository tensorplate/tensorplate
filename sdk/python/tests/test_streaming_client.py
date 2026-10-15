"""The streaming speech client, against the in-process server of ``stream_server.py``.

Every session here runs over a loopback gRPC channel. The server refuses
what the schema does not allow, so a client that sends it fails these tests
through the server's answer.
"""

from __future__ import annotations

import array
import asyncio
import contextlib
import dataclasses
import gc
import inspect
import os
import re
import typing
from collections.abc import Awaitable, Callable, Coroutine, Iterator
from typing import Any

import pytest

from tensorplate.errors import (
    ErrorCode,
    MissingDependencyError,
    ProtocolError,
    RequestTimeoutError,
    TensorPlateError,
    TransportError,
)

try:
    import tensorplate.speech as speech
except MissingDependencyError as missing:
    if os.environ.get("TENSORPLATE_REQUIRE_SPEECH_EXTRAS") == "1":
        raise
    pytest.skip(str(missing), allow_module_level=True)

import grpc
import grpc.aio

from stream_server import REPOSITORY, Playback, ResettingRelay, StreamServer, frames, recorded
from tensorplate.speech import _session, client
from tensorplate.speech._buffer import EventBuffer
from tensorplate.stream.v1 import session_pb2 as pb

TARGET = speech.Target("synthetic-deployment", 7, "sha256:" + "0" * 64)
PCM = speech.AudioFormat("pcm_s16le", 16000)
MULAW = speech.AudioFormat("mulaw", 8000)
STEREO = speech.AudioFormat("pcm_s16le", 48000, channels=2)
# 20 ms; the server grants one second, which is fifty of them.
FRAME = bytes(640)
# 320 ms of STEREO: one fits the 64 KiB HTTP/2 window and a second does not.
LARGE = bytes(320 * 192)
# A stall time and a heartbeat interval no run reaches, for a test that leaves the reader
# waiting, or stops the clock, and is not about that timer.
NO_STALL_MS = 3_600_000
NO_PING_MS = 3_600_000
Scenario = Callable[[StreamServer, speech.SpeechToTextSession], Awaitable[Any]]


def run(main: Callable[[], Awaitable[Any]]) -> Any:
    """Run `main` in a loop of its own; nothing may be left for asyncio to report."""
    reported: list[dict[str, Any]] = []

    async def watched() -> Any:
        asyncio.get_running_loop().set_exception_handler(
            lambda _, context: reported.append(context)
        )
        result = await main()
        # A task that died with an exception nobody took is reported when it is collected.
        gc.collect()
        await asyncio.sleep(0)
        return result

    result = asyncio.run(asyncio.wait_for(watched(), 60))
    assert reported == []
    return result


def play(scenario: Scenario, server: StreamServer | None = None, **options: Any) -> Any:
    """Run `scenario` with a served `server` and a session on it that is not yet open."""
    fake = server or StreamServer()
    options = {"language": "en", "input_format": PCM, **options}
    target = options.pop("target", TARGET)

    async def main() -> Any:
        async with fake.serving() as endpoint, speech.SpeechClient(endpoint) as stream:
            return await scenario(fake, stream.transcribe(target, **options))

    return run(main)


def deaf(fault: str = "deaf", **server: Any) -> StreamServer:
    """A server that reads nothing after the Open until `hear()`, serving STEREO."""
    return StreamServer(
        fault=fault,
        formats=((pb.AUDIO_ENCODING_PCM_S16LE, 48000, 2),),
        max_frame_ms=320,
        **server,
    )


async def taken(session: speech.SpeechToTextSession) -> list[Any]:
    return [event async for event in session]


async def collect(session: speech.SpeechToTextSession, events: list[Any]) -> None:
    """Take the session's events into `events`, which keeps them when the session fails."""
    async for event in session:
        events.append(event)


async def send(session: speech.SpeechToTextSession, count: int, frame: bytes) -> None:
    for _ in range(count):
        await session.send_audio(frame)


async def turns() -> None:
    """Let every task that can run without the server run as far as it can."""
    for _ in range(10):
        await asyncio.sleep(0)


async def at_once(call: Coroutine[Any, Any, Any]) -> BaseException | None:
    """What `call` raises without waiting for anything; a call that waits fails the test."""
    attempt = asyncio.ensure_future(call)
    await turns()
    done = attempt.done()
    attempt.cancel()
    assert done, "the call waits"
    return attempt.exception()


async def barrier(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
    """Return once the server has taken everything the session wrote before this call.

    Also once the server's session has ended, so that a test fails on what
    the server took and not on a wait.
    """
    pings = fake.bodies().count("ping")
    with contextlib.suppress(speech.SpeechError):
        await session._send(pb.ClientEvent(ping=pb.Ping()))
    await fake.until(lambda: fake.bodies().count("ping") > pings or not fake.live)


async def held_writer(session: speech.SpeechToTextSession) -> asyncio.Task[None]:
    """Start sending LARGE frames to a deaf server; return once the transport holds the second."""

    sender = asyncio.create_task(send(session, 3, LARGE))
    while session._sent < 3 or session._writer is None:
        await asyncio.sleep(0)
    return sender


async def flood(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
    """Have the server send 20 KiB of partial transcripts; return once the session reads no more."""
    for _ in range(20):
        fake.partial("x" * 1000)
    # Sixteen fit; the reader holds the seventeenth, which is its eighteenth event.
    while session._received < 18:
        await asyncio.sleep(0)


@contextlib.contextmanager
def stopped_clock() -> Iterator[Callable[[float], Awaitable[None]]]:
    """Stop the running loop's clock; the function given moves it on and lets due timers run.

    Nothing inside the block may wait for the server: the bound of `run` is on this clock.
    """
    loop = asyncio.get_running_loop()
    now = loop.time()

    async def advance(seconds: float) -> None:
        nonlocal now
        now += seconds
        await turns()

    with pytest.MonkeyPatch.context() as patch:
        patch.setattr(loop, "time", lambda: now)
        yield advance


async def fill(fake: StreamServer, session: speech.SpeechToTextSession) -> list[float | None]:
    """Have the session hold sixteen 1 KiB events, with its reader held on one of 3 KiB.

    Returns how long the session let each of the seventeen wait for room.
    """
    waits: list[float | None] = []
    put = session._events.put

    async def spy(
        item: Any, size: int, patience: float | None = None, *, audio: bool = False
    ) -> None:
        waits.append(patience)
        await put(item, size, patience, audio=audio)

    session._events.put = spy  # type: ignore[method-assign]
    for size in (*[1000] * 16, 3000):
        fake.partial("x" * size)
    while session._received < 18:
        await asyncio.sleep(0)
    return waits


async def finish(session: speech.SpeechToTextSession) -> None:
    """Have gRPC finish a call that a refused write ended with messages still unread.

    gRPC keeps such a call until its channel goes, and its end would then
    reach an event loop that has closed. Reading the messages lets the
    server's status arrive, in place of the INTERNAL the refused write left.
    """
    while await session._call._cython_call.receive_serialized_message() is not grpc.aio.EOF:
        pass
    while await session._call.code() is grpc.StatusCode.INTERNAL:
        await asyncio.sleep(0)


def test_audio_within_the_credit_then_finalize_returns_the_one_final_transcript() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            assert session.session_id == "synthetic-session-1"
            assert session.limits.speech_to_text == speech.SpeechToTextLimits(20, 40, 30000)
            assert session.limits.heartbeat_interval_ms == 10000
            assert session.remaining_duration_ms == 3600000
            for _ in range(5):
                await session.send_audio(FRAME)
            final = await session.finalize()
            totals = await session.close()
        segment = speech.TranscriptSegment(0, 100000, "synthetic transcript")
        assert final == speech.FinalTranscript(1, (segment,), 1600, finalize_sequence=7)
        assert final.text == "synthetic transcript"
        assert await taken(session) == [speech.EndpointDetected(1, 1600, "client_finalize"), final]
        assert totals == speech.SessionTotals(1600, 0, 0, 1, 0)
        assert fake.bodies() == ["open", *["audio"] * 5, "finalize"]
        wire = pb.AudioFormat(
            encoding=pb.AUDIO_ENCODING_PCM_S16LE, sample_rate_hz=16000, channels=1
        )
        assert fake.received[0].open == pb.Open(
            resolved=recorded("stt_open").open.resolved,
            mode=pb.SESSION_MODE_STT_STREAMING,
            language="en",
            input_format=wire,
        )
        assert session.unknown_events == 0

    play(scenario)


def test_a_sender_waits_for_credit_and_resumes_when_the_server_returns_it() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            sender = asyncio.create_task(send(session, 60, FRAME))
            await fake.until(lambda: fake.bodies().count("audio") == 50)
            await turns()
            await barrier(fake, session)
            assert fake.bodies().count("audio") == 50
            assert not sender.done()
            fake.consume()
            await sender
            assert (await session.finalize()).end_sample_offset == 60 * 320

    play(scenario, StreamServer(hold_credit=True))


def test_a_frame_outside_the_bounds_is_refused_before_it_is_sent() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            for data in (b"", bytes(641), bytes(642 * 2)):
                with pytest.raises(ValueError, match=r"samples|at most 40 ms"):
                    await session.send_audio(data)
            await session.send_audio(bytes(320))
            with pytest.raises(ValueError, match="shorter than 20 ms"):
                await session.send_audio(FRAME)
            await session.finalize()
            await session.send_audio(FRAME)
            assert (await session.close()).input_audio_samples == 480
        assert fake.bodies() == ["open", "audio", "finalize", "audio"]

    play(scenario)


def test_a_frame_no_session_would_send_is_refused_at_once_behind_an_unanswered_finalize() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            await session.send_audio(FRAME)
            finalizing = asyncio.create_task(session.finalize())
            # The answer is held back, and the Finalize holds the input gate until it comes.
            await fake.until(lambda: "finalize" in fake.bodies())
            assert session._input_gate.locked()
            for data in (b"", bytes(641), bytearray()):
                refusal = await at_once(session.send_audio(data))
                assert isinstance(refusal, ValueError)
                assert "whole, non-zero number of samples" in str(refusal)
            assert isinstance(await at_once(session.send_audio(7)), TypeError)  # type: ignore[arg-type]
            fake.final()
            await finalizing
        assert fake.bodies() == ["open", "audio", "finalize"]

    play(scenario, StreamServer(hold_finals=True))


def test_closing_ends_the_open_utterance_and_iteration_ends_with_the_session() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            await session.send_audio(FRAME)
            await session.send_audio(FRAME)
        segment = speech.TranscriptSegment(0, 40000, "synthetic transcript")
        events = await taken(session)
        assert events == [
            speech.PartialTranscript(1, 1, "synthetic"),
            speech.EndpointDetected(1, 640, "half_close"),
            speech.FinalTranscript(1, (segment,), 640, finalize_sequence=0),
        ]
        assert all(dataclasses.is_dataclass(event) for event in events)
        with pytest.raises(speech.SessionClosedError):
            await session.send_audio(FRAME)

    play(scenario, StreamServer(partials=True))


@pytest.mark.parametrize("fault", ["unknown_event", "audio_chunk"])
def test_a_server_event_this_session_does_not_know_is_passed_over_and_counted(fault: str) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            await session.send_audio(FRAME)
            await session.finalize()
            assert session.unknown_events == 1
        assert [type(event) for event in await taken(session)] == [
            speech.EndpointDetected,
            speech.FinalTranscript,
        ]

    play(scenario, StreamServer(fault=fault))


@pytest.mark.parametrize(
    ("server", "options", "error", "status"),
    [
        (StreamServer(), {"language": "xx"}, speech.PreconditionFailedError, "FAILED_PRECONDITION"),
        (
            StreamServer(),
            {"target": dataclasses.replace(TARGET, generation=6)},
            speech.PreconditionFailedError,
            "FAILED_PRECONDITION",
        ),
        (
            StreamServer(),
            {"target": speech.Target("bad id", 7, TARGET.descriptor_digest)},
            speech.InvalidEventError,
            "INVALID_ARGUMENT",
        ),
        (StreamServer(), {"turn_id": "t" * 65}, speech.InvalidEventError, "INVALID_ARGUMENT"),
        (StreamServer(), {"trace_id": "trac\u00e9"}, speech.InvalidEventError, "INVALID_ARGUMENT"),
        (StreamServer(admission_open=False), {}, speech.ServiceUnavailableError, "UNAVAILABLE"),
        (StreamServer(max_sessions=0), {}, speech.ResourceExhaustedError, "RESOURCE_EXHAUSTED"),
    ],
)
def test_a_refused_open_raises_the_class_of_its_status_and_admission_is_unknown(
    server: StreamServer, options: dict[str, Any], error: type[speech.SpeechError], status: str
) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        with pytest.raises(speech.SpeechError) as caught:
            await session.open()
        failure = caught.value
        assert type(failure) is error
        assert isinstance(failure, TensorPlateError)
        assert (failure.grpc_status, failure.reason, failure.code) == (status, None, None)
        assert type(failure.grpc_status) is str
        assert failure.admitted is None
        with pytest.raises(error):
            await taken(session)
        # An open whose call ends UNAVAILABLE before any event is sent once more, refused or not.
        assert fake.calls == (2 if status == "UNAVAILABLE" else 1)

    play(scenario, server, **options)


def test_a_decoded_refusal_gives_its_reason_and_states_that_nothing_was_admitted(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    refusal = recorded("refused_session_count_limit")
    monkeypatch.setattr(_session, "decode_refusal", lambda trailing_metadata: refusal)

    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        with pytest.raises(speech.ResourceExhaustedError) as caught:
            await session.open()
        failure = caught.value
        assert (failure.reason, failure.code) == (
            "session_count_limit",
            ErrorCode.RESOURCE_EXHAUSTED,
        )
        assert failure.admitted is False

    play(scenario, StreamServer(max_sessions=0))


@pytest.mark.parametrize(
    ("reason", "error", "code", "status"),
    [
        ("worker_shutdown", speech.ServiceUnavailableError, ErrorCode.UNAVAILABLE, "UNAVAILABLE"),
        ("backend_reset", speech.ServiceUnavailableError, ErrorCode.UNAVAILABLE, "UNAVAILABLE"),
        ("idle_timeout", speech.SessionTimeoutError, ErrorCode.TIMEOUT, "DEADLINE_EXCEEDED"),
    ],
)
def test_a_session_the_server_ends_raises_its_cause_from_every_pending_call(
    reason: str, error: type[speech.SpeechError], code: ErrorCode, status: str
) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        await session.send_audio(FRAME)
        finalize = asyncio.create_task(session.finalize())
        await fake.until(lambda: "finalize" in fake.bodies())
        fake.end(reason)
        failures = []
        for pending in (finalize, taken(session), session.send_audio(FRAME), session.close()):
            with pytest.raises(speech.SpeechError) as caught:
                await pending
            failures.append(caught.value)
        assert all(type(failure) is error for failure in failures)
        assert (failures[0].reason, failures[0].code) == (reason, code)
        assert (failures[0].admitted, failures[0].grpc_status) == (True, status)
        assert fake.calls == 1

    play(scenario, StreamServer(hold_finals=True))


@pytest.mark.parametrize(
    ("reason", "error", "code"),
    [
        ("invalid_event", speech.InvalidEventError, ErrorCode.CONFIG_INVALID),
        ("target_unresolved", speech.PreconditionFailedError, ErrorCode.UNSUPPORTED),
        ("slow_consumer", speech.ResourceExhaustedError, ErrorCode.RESOURCE_EXHAUSTED),
        ("idle_timeout", speech.SessionTimeoutError, ErrorCode.TIMEOUT),
        ("worker_shutdown", speech.ServiceUnavailableError, ErrorCode.UNAVAILABLE),
        ("client_cancelled", speech.SessionCancelledError, ErrorCode.CANCELLED),
        # One of the few reasons whose status, FAILED_PRECONDITION, is not its code's.
        ("illegal_transition", speech.ServiceUnavailableError, ErrorCode.NOT_READY),
    ],
)
def test_a_write_that_races_the_end_of_the_call_raises_the_class_of_the_cause(
    reason: str, error: type[speech.SpeechError], code: ErrorCode, monkeypatch: pytest.MonkeyPatch
) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        read = session._call.read

        async def read_once_the_call_has_a_status() -> Any:
            # The server's status is withheld, so the first the call has is the failed write's.
            if session._closed is not None:
                await session._call.code()
            return await read()

        monkeypatch.setattr(session._call, "read", read_once_the_call_has_a_status)
        reader = asyncio.create_task(taken(session))
        # The server ends the session while the transport holds the second frame. gRPC
        # lets that write return and refuses the third, and reports the call as INTERNAL.
        sender = await held_writer(session)
        fake.end(reason)
        failures = []
        for pending in (sender, reader, session.close()):
            with pytest.raises(speech.SpeechError) as caught:
                await pending
            failures.append(caught.value)
        assert [type(failure) for failure in failures] == [error] * 3
        assert failures[1] is failures[2]
        for failure in failures:
            assert (failure.grpc_status, failure.reason) == ("INTERNAL", reason)
            assert (failure.code, failure.admitted) == (code, True)
        await finish(session)

    play(scenario, deaf("deaf_status_withheld"), input_format=STEREO)


def test_a_write_that_races_the_end_of_the_call_and_loses_its_cause_is_a_server_error() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        # The reader is held by its full buffer, so SessionClosed is still unread when the
        # refused write ends the call for gRPC, which then gives the reader nothing more.
        await flood(fake, session)
        sender = await held_writer(session)
        fake.end("worker_shutdown")
        for pending in (sender, session.close()):
            with pytest.raises(speech.ServerInternalError) as caught:
                await pending
            failure = caught.value
            assert (failure.grpc_status, failure.reason, failure.code) == ("INTERNAL", None, None)
            assert failure.admitted is True
        await finish(session)

    play(scenario, deaf(output_stall_timeout_ms=NO_STALL_MS), input_format=STEREO)


@pytest.mark.parametrize(
    ("code", "error"),
    [
        (ErrorCode.CONFIG_INVALID, speech.InvalidEventError),
        (ErrorCode.SHAPE_MISMATCH, speech.InvalidEventError),
        (ErrorCode.UNSUPPORTED, speech.PreconditionFailedError),
        (ErrorCode.OOM_ERROR, speech.ResourceExhaustedError),
        (ErrorCode.RESOURCE_EXHAUSTED, speech.ResourceExhaustedError),
        (ErrorCode.TIMEOUT, speech.SessionTimeoutError),
        (ErrorCode.LOAD_FAILED, speech.ServiceUnavailableError),
        (ErrorCode.NOT_READY, speech.ServiceUnavailableError),
        (ErrorCode.UNAVAILABLE, speech.ServiceUnavailableError),
        (ErrorCode.CANCELLED, speech.SessionCancelledError),
        (ErrorCode.INFERENCE_FAILED, speech.ServerInternalError),
        (ErrorCode.INTERNAL, speech.ServerInternalError),
    ],
)
def test_the_class_follows_the_causes_code_only_for_internal_after_a_failed_write(
    code: ErrorCode, error: type[speech.SpeechError]
) -> None:
    cause = pb.EndCause(code=pb.ErrorCode.Value(f"ERROR_CODE_{code.name}"))
    internal, unavailable = grpc.StatusCode.INTERNAL, grpc.StatusCode.UNAVAILABLE

    raced = _session.failure_of(internal, cause, True, write_failed=True)
    assert type(raced) is error
    assert (raced.code, raced.grpc_status) == (code, "INTERNAL")
    # A status the client read stands, and so does INTERNAL when no write failed.
    assert type(_session.failure_of(internal, cause, True)) is speech.ServerInternalError
    stated = _session.failure_of(unavailable, cause, True, write_failed=True)
    assert type(stated) is speech.ServiceUnavailableError
    assert type(_session.failure_of(internal, None, True, write_failed=True)) is (
        speech.ServerInternalError
    )


def test_cancel_tells_the_server_and_drops_what_was_not_taken() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        await session.send_audio(FRAME)
        while session._events.used == 0:
            await asyncio.sleep(0)
        await session.cancel()
        assert await taken(session) == []
        assert fake.bodies()[-1] == "cancel"
        assert fake.closed[-1].cause == recorded("closed_client_cancelled").session_closed.cause
        for call in (session.send_audio(FRAME), session.finalize(), session.close()):
            with pytest.raises(speech.SessionClosedError):
                await call
        await session.cancel()

    play(scenario, StreamServer(partials=True))


def test_leaving_the_session_on_an_exception_cancels_it() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        with pytest.raises(KeyError):
            async with session:
                raise KeyError("the caller's own failure")
        assert fake.bodies() == ["open", "cancel"]

    play(scenario)


def test_ping_is_sent_at_the_interval_ready_gives() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            while fake.bodies().count("ping") < 3:
                await asyncio.sleep(0.005)
            await session.send_audio(FRAME)
            await session.finalize()

    play(scenario, StreamServer(heartbeat_interval_ms=10))


def test_an_open_that_is_not_answered_in_time_ends_with_admission_unknown() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        with pytest.raises(speech.SessionTimeoutError) as caught:
            await session.open()
        assert (caught.value.admitted, caught.value.grpc_status) == (None, None)

    play(scenario, StreamServer(fault="silent"), open_timeout=0.05)


def test_a_finalize_that_is_not_answered_in_time_ends_the_session() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        await session.send_audio(FRAME)
        with pytest.raises(speech.SessionTimeoutError) as caught:
            await session.finalize(timeout=0.05)
        assert caught.value.admitted is True
        with pytest.raises(speech.SessionTimeoutError):
            await session.send_audio(FRAME)

    play(scenario, StreamServer(hold_finals=True))


@pytest.mark.parametrize(
    "fault",
    [
        "sequence_gap",
        "event_before_ready",
        "other_generation",
        "no_credit",
        "final_without_endpoint",
        "partial_after_endpoint",
        "other_finalize",
        "second_endpoint",
        "ok_with_cause",
        "ok_without_closed",
    ],
)
def test_a_server_that_breaks_the_protocol_ends_the_session_with_a_protocol_error(
    fault: str,
) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async def use() -> None:
            async with session:
                await session.send_audio(FRAME)
                if fault == "ok_with_cause":
                    fake.end("worker_shutdown")
                await session.finalize()

        with pytest.raises(speech.StreamProtocolError) as caught:
            await use()
        assert isinstance(caught.value, ProtocolError)
        assert caught.value.grpc_status is None

    play(scenario, StreamServer(fault=fault))


def test_a_call_lost_after_ready_raises_the_class_of_its_status_as_admitted() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        with pytest.raises(speech.ServiceUnavailableError) as caught:
            await session.close()
        assert (caught.value.admitted, caught.value.reason) == (True, None)

    play(scenario, StreamServer(fault="drop"))


def test_an_endpoint_that_cannot_be_reached_is_unavailable() -> None:
    async def main() -> None:
        async with speech.SpeechClient("127.0.0.1:1") as stream:
            session = stream.transcribe(TARGET, language="en", input_format=PCM)
            with pytest.raises(speech.ServiceUnavailableError) as caught:
                await session.open()
            assert caught.value.admitted is None

    run(main)


RelayScenario = Callable[[StreamServer, ResettingRelay, speech.SpeechClient], Awaitable[Any]]


def relayed(scenario: RelayScenario, server: StreamServer | None = None) -> Any:
    """Run `scenario` with a client whose connections pass through a `ResettingRelay`."""
    fake = server or StreamServer()

    async def main() -> Any:
        async with fake.serving() as endpoint:
            relay = ResettingRelay(endpoint)
            async with relay.serving() as through, speech.SpeechClient(through) as stream:
                return await scenario(fake, relay, stream)

    return run(main)


def endings(monkeypatch: pytest.MonkeyPatch) -> list[tuple[str, bool]]:
    """The status and whether a write failed, of each call that ends with an error."""
    seen: list[tuple[str, bool]] = []
    failure_of = _session.failure_of

    def recording(status: grpc.StatusCode, *arguments: Any, **options: Any) -> Any:
        seen.append((status.name, options.get("write_failed", False)))
        return failure_of(status, *arguments, **options)

    monkeypatch.setattr(_session, "failure_of", recording)
    return seen


async def warm(stream: speech.SpeechClient) -> None:
    """Run one session, so that the client's connection is established before a test resets it."""
    async with stream.transcribe(TARGET, language="en", input_format=PCM) as session:
        await session.send_audio(FRAME)


class LateWrite:
    """A session's first call, whose first write waits for the call to end.

    With `internal` the call reports what gRPC does when that write is issued
    while gRPC reads the reset (seen against a relay, and not forced there):
    the write fails and the status is INTERNAL.
    """

    def __init__(self, call: Any, internal: bool) -> None:
        self._call, self._internal = call, internal

    def __getattr__(self, name: str) -> Any:
        return getattr(self._call, name)

    async def write(self, event: Any) -> None:
        await self._call.code()
        if self._internal:
            raise grpc.aio.AioRpcError(
                grpc.StatusCode.INTERNAL,
                grpc.aio.Metadata(),
                grpc.aio.Metadata(),
                details="Internal error from Core",
            )
        await self._call.write(event)

    async def code(self) -> grpc.StatusCode:
        status = await self._call.code()
        return grpc.StatusCode.INTERNAL if self._internal else status


@pytest.mark.parametrize(
    ("frame", "write", "ending"),
    [
        ("data", "", ("UNAVAILABLE", False)),
        ("headers", "after", ("UNAVAILABLE", True)),
        ("headers", "during", ("INTERNAL", True)),
    ],
)
def test_an_open_whose_connection_is_reset_before_any_answer_is_sent_once_more(
    frame: str, write: str, ending: tuple[str, bool], monkeypatch: pytest.MonkeyPatch
) -> None:
    ended = endings(monkeypatch)

    async def scenario(fake: StreamServer, relay: ResettingRelay, stream: Any) -> None:
        await warm(stream)
        relay.arm(frame)
        session = stream.transcribe(TARGET, language="en", input_format=PCM)
        if write:
            connect = session._connect
            made: list[Any] = []

            def connecting() -> Any:
                made.append(connect())
                return LateWrite(made[0], write == "during") if len(made) == 1 else made[-1]

            monkeypatch.setattr(session, "_connect", connecting)
        await session.open()
        # A failed write of the first call would change how the second one's INTERNAL reads,
        # and must not lift the bound on the events the second call holds.
        assert not session._write_failed
        assert session._events._capacity[False] == _session.METADATA_BUFFER_BYTES
        await session.send_audio(FRAME)
        totals = await session.close()
        assert totals.input_audio_samples == len(FRAME) // 2
        events = [type(event) for event in await taken(session)]
        assert events == [speech.EndpointDetected, speech.FinalTranscript]
        assert ended == [ending]
        assert (relay.resets, relay.connections, relay.calls) == (1, 2, 3)

    relayed(scenario)


def test_a_channel_closed_before_the_second_open_ends_the_session(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def scenario(fake: StreamServer, relay: ResettingRelay, stream: Any) -> None:
        await warm(stream)
        relay.arm("data")
        session = stream.transcribe(TARGET, language="en", input_format=PCM)
        connect = session._connect
        made: list[Any] = []

        def connecting() -> Any:
            if made:
                raise grpc.aio.UsageError("Channel is closed.")
            made.append(connect())
            return made[0]

        monkeypatch.setattr(session, "_connect", connecting)
        with pytest.raises(speech.SessionClosedError):
            await session.open()
        assert session._ended
        assert await taken(session) == []

    relayed(scenario)


def test_an_open_is_sent_at_most_twice() -> None:
    async def scenario(fake: StreamServer, relay: ResettingRelay, stream: Any) -> None:
        await warm(stream)
        relay.arm("data", times=3)
        session = stream.transcribe(TARGET, language="en", input_format=PCM)
        with pytest.raises(speech.ServiceUnavailableError) as caught:
            await session.open()
        assert caught.value.admitted is None
        assert (relay.resets, relay.connections, relay.calls) == (2, 2, 3)

    relayed(scenario)


@pytest.mark.parametrize(
    ("fault", "error"),
    [
        ("drop_before_ready", speech.ServiceUnavailableError),
        ("internal", speech.ServerInternalError),
    ],
)
def test_an_open_answered_before_its_loss_or_ended_internal_by_the_server_is_not_sent_again(
    fault: str, error: type[speech.SpeechError]
) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        with pytest.raises(error):
            await session.open()
        assert fake.calls == 1

    play(scenario, StreamServer(fault=fault))


def hold_the_loss(
    monkeypatch: pytest.MonkeyPatch, session: speech.SpeechToTextSession
) -> tuple[asyncio.Event, asyncio.Event]:
    """Hold the open where its first call has been lost, before it makes the second, until
    `release` is set."""
    held, release = asyncio.Event(), asyncio.Event()
    until = session._until

    async def holding(done: Callable[[], bool]) -> None:
        try:
            await until(done)
        except speech.SpeechError:
            if session._lost is None or held.is_set():
                raise
            held.set()
            await release.wait()
            # The open resumes on the session's state as it is now.
            await until(done)

    monkeypatch.setattr(session, "_until", holding)
    return held, release


@pytest.mark.parametrize("end", ["cancel", "aclose"])
def test_a_session_cancelled_or_closed_while_its_open_holds_a_lost_call_ends_with_no_second_open(
    end: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    async def scenario(fake: StreamServer, relay: ResettingRelay, stream: Any) -> None:
        await warm(stream)
        relay.arm("data")
        session = stream.transcribe(TARGET, language="en", input_format=PCM)
        held, release = hold_the_loss(monkeypatch, session)
        opening = asyncio.create_task(session.open())
        await held.wait()
        assert not session._ended
        await (session.cancel() if end == "cancel" else stream.aclose())
        assert session._ended
        release.set()
        with pytest.raises(speech.SessionClosedError):
            await opening
        assert not session._reopened
        assert await taken(session) == []
        assert (relay.connections, relay.calls) == (1, 2)

    relayed(scenario)


@pytest.mark.parametrize("end", ["timeout", "caller_cancelled"])
def test_an_open_that_ends_while_it_holds_a_lost_call_ends_the_session(
    end: str, monkeypatch: pytest.MonkeyPatch
) -> None:
    async def scenario(fake: StreamServer, relay: ResettingRelay, stream: Any) -> None:
        await warm(stream)
        relay.arm("data")
        session = stream.transcribe(TARGET, language="en", input_format=PCM, open_timeout=0.2)
        held, _ = hold_the_loss(monkeypatch, session)
        opening = asyncio.create_task(session.open())
        if end == "timeout":
            with pytest.raises(speech.SessionTimeoutError):
                await opening
        else:
            await held.wait()
            opening.cancel()
            with pytest.raises(asyncio.CancelledError):
                await opening
            # The session ends with the lost call's failure.
            with pytest.raises(speech.ServiceUnavailableError):
                await taken(session)
        assert session._ended
        assert not session._reopened
        assert (relay.connections, relay.calls) == (1, 2)

    relayed(scenario)


def test_an_iterator_waiting_while_the_open_holds_a_lost_call_takes_the_second_calls_events(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def scenario(fake: StreamServer, relay: ResettingRelay, stream: Any) -> None:
        await warm(stream)
        relay.arm("data")
        session = stream.transcribe(TARGET, language="en", input_format=PCM)
        reading = asyncio.create_task(taken(session))
        held, release = hold_the_loss(monkeypatch, session)
        opening = asyncio.create_task(session.open())
        await held.wait()
        for _ in range(3):
            await asyncio.sleep(0)
        assert not reading.done()
        release.set()
        await opening
        await session.send_audio(FRAME)
        await session.close()
        events = [type(event) for event in await reading]
        assert events == [speech.EndpointDetected, speech.FinalTranscript]

    relayed(scenario)


class HeldEnd:
    """A session's call whose status is given only once `release` is set."""

    def __init__(self, call: Any, asked: asyncio.Event, release: asyncio.Event) -> None:
        self._call, self._asked, self._release = call, asked, release

    def __getattr__(self, name: str) -> Any:
        return getattr(self._call, name)

    async def code(self) -> grpc.StatusCode:
        self._asked.set()
        await self._release.wait()
        status: grpc.StatusCode = await self._call.code()
        return status


def test_a_first_call_lost_after_its_open_returned_ends_the_session(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def scenario(fake: StreamServer, relay: ResettingRelay, stream: Any) -> None:
        await warm(stream)
        relay.arm("data")
        session = stream.transcribe(TARGET, language="en", input_format=PCM)
        connect = session._connect
        asked, release = asyncio.Event(), asyncio.Event()
        monkeypatch.setattr(session, "_connect", lambda: HeldEnd(connect(), asked, release))
        opening = asyncio.create_task(session.open())
        # The first call has ended; the reader waits for its status.
        await asked.wait()
        opening.cancel()
        with pytest.raises(asyncio.CancelledError):
            await opening
        release.set()
        assert session._reader is not None
        await session._reader
        assert session._ended
        assert (relay.connections, relay.calls) == (1, 2)

    relayed(scenario)


def test_a_first_call_lost_while_the_open_times_out_raises_the_timeout(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def scenario(fake: StreamServer, relay: ResettingRelay, stream: Any) -> None:
        await warm(stream)
        relay.arm("data")
        session = stream.transcribe(TARGET, language="en", input_format=PCM, open_timeout=0.2)
        connect = session._connect
        asked, release, aborted = asyncio.Event(), asyncio.Event(), asyncio.Event()
        monkeypatch.setattr(session, "_connect", lambda: HeldEnd(connect(), asked, release))
        abort = session._abort

        def aborting(error: speech.SpeechError) -> None:
            aborted.set()
            abort(error)

        monkeypatch.setattr(session, "_abort", aborting)
        opening = asyncio.create_task(session.open())
        # The first call is lost, and the open's time runs out before its status is read.
        await asked.wait()
        await aborted.wait()
        release.set()
        with pytest.raises(speech.SessionTimeoutError):
            await opening
        assert session._ended
        assert (relay.connections, relay.calls) == (1, 2)

    relayed(scenario)


def test_a_first_call_lost_while_the_session_is_cancelled_is_not_sent_again() -> None:
    async def scenario(fake: StreamServer, relay: ResettingRelay, stream: Any) -> None:
        session = stream.transcribe(TARGET, language="en", input_format=PCM)
        opening = asyncio.create_task(session.open())
        await fake.until(lambda: len(fake.received) == 1)
        # The server does not answer the Cancel; the connection is lost while the cancel waits.
        cancelling = asyncio.create_task(session.cancel(timeout=30))
        await asyncio.sleep(0)
        assert session._cancelling
        relay.reset()
        await cancelling
        with pytest.raises(speech.SessionClosedError):
            await opening
        assert session._ended
        assert (relay.connections, relay.calls) == (1, 1)

    relayed(scenario, StreamServer(fault="silent"))


def test_a_session_cancelled_during_its_first_open_sends_no_second_open() -> None:
    async def scenario(fake: StreamServer, relay: ResettingRelay, stream: Any) -> None:
        session = stream.transcribe(TARGET, language="en", input_format=PCM, cancel_timeout=0.05)
        opening = asyncio.create_task(session.open())
        await fake.until(lambda: len(fake.received) == 1)
        await session.cancel()
        with pytest.raises(speech.SessionClosedError):
            await opening
        assert (relay.connections, relay.calls) == (1, 1)
        assert [event.WhichOneof("body") for event in fake.received] == ["open"]

    relayed(scenario, StreamServer(fault="silent"))


def test_input_before_ready_is_not_written_so_an_open_sent_again_carried_only_the_open() -> None:
    async def scenario(fake: StreamServer, relay: ResettingRelay, stream: Any) -> None:
        session = stream.transcribe(TARGET, language="en", input_format=PCM, cancel_timeout=0.05)
        opening = asyncio.create_task(session.open())
        await fake.until(lambda: len(fake.received) == 1)
        for offered in (session.send_audio(FRAME), session.finalize(), session.close()):
            with pytest.raises(speech.SessionClosedError):
                await offered
        assert relay.data_frames == 1
        relay.reset()
        await fake.until(lambda: len(fake.received) == 2)
        assert [event.WhichOneof("body") for event in fake.received] == ["open", "open"]
        assert relay.data_frames == 2
        await session.cancel()
        with pytest.raises(speech.SessionClosedError):
            await opening

    relayed(scenario, StreamServer(fault="silent"))


def test_two_tasks_sending_at_once_keep_one_order_on_the_wire() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async def send() -> None:
            for _ in range(20):
                await session.send_audio(FRAME)

        def ping() -> Any:
            return session._send(pb.ClientEvent(ping=pb.Ping()))

        async with session:
            await asyncio.gather(send(), send(), *(ping() for _ in range(20)))
            assert (await session.finalize()).end_sample_offset == 40 * 320
        assert fake.bodies().count("ping") == 20

    play(scenario)


def test_cancel_ends_a_session_whose_writer_the_transport_holds_back() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        sender = await held_writer(session)
        assert session._write_lock.locked()
        await session.cancel(timeout=0.1)
        with pytest.raises(speech.SessionClosedError):
            await sender

    play(scenario, deaf(), input_format=STEREO)


def test_the_channel_keeps_the_http2_window_fixed_and_bounds_message_size(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    built: list[dict[str, Any]] = []
    channel = grpc.aio.insecure_channel

    def build(endpoint: str, options: Any = None) -> Any:
        built.append(dict(options))
        return channel(endpoint, options=options)

    monkeypatch.setattr(grpc.aio, "insecure_channel", build)

    async def main() -> None:
        async with speech.SpeechClient("127.0.0.1:1"):
            pass

    asyncio.run(main())
    assert built == [
        {
            "grpc.http2.bdp_probe": 0,
            "grpc.max_send_message_length": client.MAX_MESSAGE_BYTES,
            "grpc.max_receive_message_length": client.MAX_MESSAGE_BYTES,
        }
    ]
    assert client.MAX_MESSAGE_BYTES == 64 * 1024


def test_the_event_buffer_holds_back_a_put_while_it_is_full() -> None:
    async def main() -> None:
        buffer: EventBuffer[str] = EventBuffer(10)
        await buffer.put("large", 25)
        blocked = asyncio.create_task(buffer.put("next", 4))
        await turns()
        assert not blocked.done()
        assert buffer.used == 25
        assert await buffer.get() == "large"
        await blocked
        await buffer.put("more", 6)
        assert buffer.used == 10
        late = asyncio.create_task(buffer.put("late", 1))
        buffer.end()
        await late
        assert [await buffer.get(), await buffer.get(), await buffer.get()] == [
            "next",
            "more",
            None,
        ]
        buffer.end(discard=True)
        assert buffer.used == 0

    run(main)


def test_the_event_buffer_lets_a_held_put_through_once_unbound() -> None:
    async def main() -> None:
        buffer: EventBuffer[str] = EventBuffer(10)
        await buffer.put("first", 10)
        with pytest.raises(asyncio.TimeoutError):
            await buffer.put("refused", 1, 0.01)
        held = asyncio.create_task(buffer.put("held", 1, 30))
        await turns()
        assert not held.done()
        buffer.unbound()
        await held
        await buffer.put("more", 100)
        assert buffer.used == 111

    run(main)


@pytest.mark.parametrize("turns_before_the_take", [0, 1, 2, 3])
def test_a_take_admits_the_put_that_waits_whichever_turn_its_wait_began_on(
    turns_before_the_take: int,
) -> None:
    # Before Python 3.12 ``asyncio.wait_for`` starts what it is given one turn of the loop
    # late, so a take on the turn the put found the buffer full comes before that start.
    async def main() -> None:
        buffer: EventBuffer[str] = EventBuffer(10)
        await buffer.put("held", 10)
        parked = asyncio.create_task(buffer.put("parked", 1, 30))
        for _ in range(turns_before_the_take):
            await asyncio.sleep(0)
        assert await buffer.get() == "held"
        await turns()
        admitted = parked.done()
        parked.cancel()
        assert admitted
        assert buffer.used == 1

    run(main)


def test_the_recorded_session_is_what_the_client_sends_and_what_it_reads() -> None:
    session_frames = frames("stt_session")
    sent = [(name, encoded) for name, kind, encoded in session_frames if kind == "ClientEvent"]
    audio = [recorded(name).audio.data for name, _ in sent if name.startswith("stt_audio")]
    opening = recorded("stt_open").open
    playback = Playback("stt_session")

    async def main() -> tuple[Any, ...]:
        async with playback.serving() as endpoint, speech.SpeechClient(endpoint) as stream:
            session = stream.transcribe(
                TARGET,
                language=opening.language,
                input_format=MULAW,
                trace_id=opening.trace_id,
                turn_id=opening.turn_id,
            )
            async with session:
                reader = asyncio.create_task(taken(session))
                for data in audio[:5]:
                    await session.send_audio(data)
                second = await session.finalize()
                await session.send_audio(audio[5])
                third = await session.finalize()
                await session.send_audio(audio[6])
                await session.send_audio(audio[7])
                totals = await session.close()
            return await reader, second, third, totals

    events, second, third, totals = run(main)

    assert playback.received == [recorded(name) for name, _ in sent]
    assert [event.SerializeToString() for event in playback.received] == [e for _, e in sent]
    part = speech.TranscriptSegment
    finals = [
        speech.FinalTranscript(
            1, (part(0, 30000, "synthetic"), part(30000, 60000, " transcript")), 480, 0
        ),
        speech.FinalTranscript(
            2, (part(75000, 100000, "synthetic"), part(100000, 110000, " words")), 880, 7
        ),
        speech.FinalTranscript(3, (), 1040, 9),
        speech.FinalTranscript(4, (part(135000, 160000, "synthetic end"),), 1280, 0),
    ]
    assert events == [
        speech.PartialTranscript(1, 1, "synthetic"),
        speech.PartialTranscript(1, 2, "synthetic transcript"),
        speech.EndpointDetected(1, 480, "silence"),
        finals[0],
        speech.PartialTranscript(2, 1, "synthetic words"),
        speech.EndpointDetected(2, 880, "client_finalize"),
        finals[1],
        speech.EndpointDetected(3, 1040, "client_finalize"),
        finals[2],
        speech.PartialTranscript(4, 1, "synthetic end"),
        speech.EndpointDetected(4, 1280, "half_close"),
        finals[3],
    ]
    assert (second, third) == (finals[1], finals[2])
    assert [final.text for final in finals[1:3]] == ["synthetic words", ""]
    assert totals == speech.SessionTotals(1280, 0, 0, 4, 0)


@pytest.mark.parametrize(
    ("steps", "order"),
    [
        (("final", "endpoint", "final"), [1, 1, 2, 2]),
        # The next utterance's endpoint may precede this one's final transcript.
        (("endpoint", "final", "final"), [1, 2, 1, 2]),
    ],
)
def test_a_final_transcript_that_names_no_finalize_answers_none(
    steps: tuple[str, ...], order: list[int]
) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            reader = asyncio.create_task(taken(session))
            await session.send_audio(FRAME)
            await fake.until(lambda: "audio" in fake.bodies())
            fake.endpoint()
            finalizing = asyncio.create_task(session.finalize())
            await fake.until(lambda: "finalize" in fake.bodies())
            for step in steps:
                getattr(fake, step)()
            answer = await finalizing
        events = await reader
        assert answer == speech.FinalTranscript(2, (), 320, finalize_sequence=3)
        assert [event.utterance_id for event in events] == order
        assert [event.reason for event in events if isinstance(event, speech.EndpointDetected)] == [
            "silence",
            "client_finalize",
        ]
        finals = [event for event in events if isinstance(event, speech.FinalTranscript)]
        assert [final.finalize_sequence for final in finals] == [0, 3]

    play(scenario, StreamServer(hold_finals=True))


def test_a_cancelled_finalize_holds_audio_back_until_its_answer() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            await session.send_audio(FRAME)
            finalizing = asyncio.create_task(session.finalize())
            await fake.until(lambda: "finalize" in fake.bodies())
            finalizing.cancel()
            await asyncio.wait({finalizing})
            assert finalizing.cancelled()
            sender = asyncio.create_task(session.send_audio(FRAME))
            await turns()
            # The server holds the answer back and has taken a Ping written after the frame
            # was given to the session: no audio reached it behind the Finalize.
            await barrier(fake, session)
            assert fake.bodies() == ["open", "audio", "finalize", "ping"]
            assert not sender.done()
            fake.final()
            await sender
            closing = asyncio.create_task(session.close())
            await fake.until(lambda: fake.half_closed)
            fake.final()
            totals = await closing
        # The server refuses audio while a Finalize is unanswered, so it took none before.
        assert fake.bodies() == ["open", "audio", "finalize", "ping", "audio"]
        assert totals == speech.SessionTotals(640, 0, 0, 2, 0)

    play(scenario, StreamServer(hold_finals=True))


def test_a_finalize_waits_for_the_audio_that_is_ahead_of_it() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            sender = asyncio.create_task(send(session, 51, FRAME))
            await fake.until(lambda: fake.bodies().count("audio") == 50)
            # The server can take frame 50 before the sender is back with frame 51.
            while not (session._input_gate.locked() and session._samples_sent == 50 * 320):
                await asyncio.sleep(0)
            finalizing = asyncio.create_task(session.finalize())
            await turns()
            await barrier(fake, session)
            assert "finalize" not in fake.bodies()
            fake.consume()
            await sender
            await fake.until(lambda: "finalize" in fake.bodies())
            fake.final()
            assert (await finalizing).end_sample_offset == 51 * 320
        assert fake.bodies() == ["open", *["audio"] * 50, "ping", "audio", "finalize"]

    play(scenario, StreamServer(hold_credit=True, hold_finals=True))


def test_a_sender_cancelled_while_it_waits_for_credit_sends_nothing() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            sender = asyncio.create_task(send(session, 51, FRAME))
            await fake.until(lambda: fake.bodies().count("audio") == 50)
            await turns()
            sender.cancel()
            await asyncio.wait({sender})
            assert sender.cancelled()
            fake.consume()
            await session.send_audio(FRAME)
            assert (await session.finalize()).end_sample_offset == 51 * 320
        assert fake.bodies().count("audio") == 51

    play(scenario, StreamServer(hold_credit=True))


def test_a_second_finalize_waits_for_the_answer_to_the_first() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            await session.send_audio(FRAME)
            first = asyncio.create_task(session.finalize())
            await fake.until(lambda: "finalize" in fake.bodies())
            first.cancel()
            await asyncio.wait({first})
            second = asyncio.create_task(session.finalize(timeout=30))
            await turns()
            await barrier(fake, session)
            assert fake.bodies().count("finalize") == 1
            fake.final()
            await fake.until(lambda: fake.bodies().count("finalize") == 2)
            fake.final()
            answer = await second
        assert (answer.utterance_id, answer.finalize_sequence) == (2, 5)

    play(scenario, StreamServer(hold_finals=True))


def test_close_stops_the_ping_task_before_the_session_ends() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        await session.send_audio(FRAME)
        closing = asyncio.create_task(session.close())
        await fake.until(lambda: fake.half_closed)
        assert session._pinger is not None
        assert session._pinger.done()
        fake.final()
        assert (await closing).utterances_completed == 1

    play(scenario, StreamServer(hold_finals=True))


def test_a_ready_without_a_heartbeat_interval_is_refused_at_open() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        with pytest.raises(speech.StreamProtocolError, match="no heartbeat interval") as caught:
            await session.open()
        assert caught.value.admitted is None
        assert session._pinger is None
        await fake.until(lambda: fake.live == 0)
        assert "ping" not in fake.bodies()

    play(scenario, StreamServer(heartbeat_interval_ms=0))


USES: dict[str, Callable[[speech.SpeechToTextSession], Coroutine[Any, Any, Any]]] = {
    "iteration": taken,
    "close": lambda session: session.close(),
    "finalize": lambda session: session.finalize(timeout=30),
    "send_audio": lambda session: session.send_audio(FRAME),
}


@pytest.mark.parametrize("use", [*USES, "exit"])
def test_a_frame_that_does_not_decode_ends_the_session_for_every_use(use: str) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        if use == "exit":
            with pytest.raises(speech.StreamProtocolError):
                async with session:
                    await session._until_ended()
            return
        await session.open()
        await session._until_ended()
        with pytest.raises(speech.StreamProtocolError, match="not a server event") as caught:
            await USES[use](session)
        assert (caught.value.admitted, caught.value.grpc_status) == (True, None)
        assert caught.value.__cause__ is None
        await session.cancel()

    play(scenario, StreamServer(fault="undecodable"))


@pytest.mark.parametrize("use", ["iteration", "close", "finalize", "cancel"])
def test_a_use_that_waits_is_woken_by_a_frame_that_does_not_decode(use: str) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        if use == "cancel":
            waiting = asyncio.create_task(session.cancel())
            await fake.until(lambda: fake.live == 1)
            await turns()
            fake.hear()
            await waiting
            assert await taken(session) == []
            return
        waiting = asyncio.create_task(USES[use](session))
        await turns()
        fake.hear()
        with pytest.raises(speech.StreamProtocolError):
            await waiting

    play(scenario, StreamServer(fault="deaf_undecodable"))


def test_a_reader_that_fails_ends_the_session_with_the_failure_as_its_cause(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    failure = RuntimeError("a defect in the client")

    def fail(event: Any) -> None:
        raise failure

    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        monkeypatch.setattr(session, "_task_event", fail)
        await session.open()
        await session.send_audio(FRAME)
        for call in (session.finalize(), session.close(), taken(session)):
            with pytest.raises(speech.StreamProtocolError, match="RuntimeError") as caught:
                await call
            assert caught.value.__cause__ is failure
        await session.cancel()

    play(scenario, StreamServer(partials=True))


def test_a_reader_cancelled_while_it_is_held_still_ends_the_session() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        await flood(fake, session)
        assert session._reader is not None
        session._reader.cancel()
        with pytest.raises(speech.SessionCancelledError):
            await session.close(timeout=1)
        await fake.until(lambda: fake.live == 0)

    play(scenario, StreamServer(output_stall_timeout_ms=NO_STALL_MS))


def documented(**server: Any) -> tuple[dict[str, Any], StreamServer]:
    """The example of the SDK's page, and a server for the deployment it names."""
    page = (REPOSITORY / "docs/sdk/python.md").read_text(encoding="utf-8")
    section = page[page.index("## Streaming speech client") :]
    example: dict[str, Any] = {}
    exec(section.split("```python\n", 1)[1].split("```", 1)[0], example)
    fake = StreamServer(deployment_id="my-deployment", generation=0, descriptor_digest="", **server)
    return example, fake


def test_the_documented_example_reads_events_while_it_sends(
    capsys: pytest.CaptureFixture[str],
) -> None:
    # Forty frames draw 40 KiB of partial transcripts, more than a session holds unread.
    example, fake = documented(partial_bytes=1000)

    async def main() -> Any:
        async with fake.serving() as endpoint:
            return await example["transcribe"](endpoint, [FRAME] * 40)

    assert run(main) == "synthetic transcript"
    assert capsys.readouterr().out.count("x" * 1000) == 40
    assert fake.bodies() == ["open", *["audio"] * 40, "finalize"]


def test_the_documented_example_raises_its_readers_error_when_reading_fails() -> None:
    # Forty frames fit the input credit and draw 40 KiB of partial transcripts. An example
    # that left its sender waiting for the Finalize's answer behind the events nobody takes
    # would raise the stall timer's error; one that stops at the reader's failure holds a
    # few events at most, and the timer never starts.
    example, fake = documented(partial_bytes=1000, output_stall_timeout_ms=50)
    failure = RuntimeError("the display failed")

    def show(text: str) -> None:
        raise failure

    example["print"] = show

    async def main() -> None:
        async with fake.serving() as endpoint:
            with pytest.raises(RuntimeError) as caught:
                await example["transcribe"](endpoint, [FRAME] * 40)
            assert caught.value is failure
            await fake.until(lambda: fake.live == 0)
            # The example left the session by its exit on the error, not by a timer.
            reasons = [closed.cause.reason for closed in fake.closed]
            assert reasons == [pb.FAILURE_REASON_CLIENT_CANCELLED]

    # `run` fails on a task whose exception nobody took.
    run(main)


def test_the_documented_example_takes_its_readers_outcome_when_the_session_fails() -> None:
    example, fake = documented(hold_finals=True)

    async def main() -> None:
        async with fake.serving() as endpoint:
            running = asyncio.create_task(example["transcribe"](endpoint, [FRAME] * 5))
            await fake.until(lambda: "finalize" in fake.bodies())
            fake.end("worker_shutdown")
            with pytest.raises(speech.ServiceUnavailableError):
                await running

    # `run` fails on a task whose exception nobody took.
    run(main)


def test_a_session_whose_events_are_left_unread_ends_with_a_typed_error() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        await flood(fake, session)
        # More than the credit, with nothing reading: the Accepted are behind the events.
        with pytest.raises(speech.ResourceExhaustedError, match="left unread") as caught:
            await send(session, 60, FRAME)
        failure = caught.value
        assert (failure.admitted, failure.grpc_status, failure.reason) == (True, None, None)
        for call in (session.finalize(), session.close()):
            with pytest.raises(speech.ResourceExhaustedError):
                await call
        events: list[Any] = []
        with pytest.raises(speech.ResourceExhaustedError):
            await collect(session, events)
        assert len(events) == 16
        await fake.until(lambda: fake.live == 0)

    play(scenario, StreamServer(output_stall_timeout_ms=50))


def test_the_stall_timer_lasts_what_ready_gives_and_restarts_with_each_event_taken() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        assert session.limits.output_stall_timeout_ms == 8000
        assert await fill(fake, session) == [8.0] * 17
        with stopped_clock() as advance:
            # Taking one event leaves no room for the one the reader holds, and restarts the
            # timer, here on the stopped clock.
            for _ in range(2):
                await anext(session)
                await turns()
                await advance(6.0)
                assert not session._ended
            await advance(1.9)
            assert not session._ended
            await advance(0.2)
            assert session._ended
        with pytest.raises(speech.ResourceExhaustedError, match=r"left unread for 8\.0 s"):
            await session.close()

    play(scenario, StreamServer(output_stall_timeout_ms=8000, heartbeat_interval_ms=NO_PING_MS))


def test_the_stall_timer_runs_only_while_the_reader_is_held_by_unread_events() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            with stopped_clock() as advance:
                await advance(9.0)
                assert not session._ended
            # Events wait to be taken, and the reader still reads: nothing waits on the caller.
            fake.partial("x" * 1000)
            while not session._events.used:
                await asyncio.sleep(0)
            with stopped_clock() as advance:
                await advance(9.0)
                assert not session._ended
            await session.send_audio(FRAME)
            assert (await session.finalize()).end_sample_offset == 320

    play(scenario, StreamServer(output_stall_timeout_ms=8000, heartbeat_interval_ms=NO_PING_MS))


def test_a_ready_without_a_stall_time_is_refused_at_open() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        with pytest.raises(speech.StreamProtocolError, match="no output stall time") as caught:
            await session.open()
        assert caught.value.admitted is None
        await fake.until(lambda: fake.live == 0)

    play(scenario, StreamServer(output_stall_timeout_ms=0))


def test_a_sender_waiting_for_credit_raises_the_cause_the_server_ends_the_session_with() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        sender = asyncio.create_task(send(session, 60, FRAME))
        await fake.until(lambda: fake.bodies().count("audio") == 50)
        await turns()
        fake.end("slow_consumer")
        with pytest.raises(speech.ResourceExhaustedError) as caught:
            await sender
        failure = caught.value
        assert (failure.reason, failure.grpc_status) == ("slow_consumer", "RESOURCE_EXHAUSTED")
        assert (failure.code, failure.admitted) == (ErrorCode.RESOURCE_EXHAUSTED, True)
        assert fake.bodies().count("audio") == 50

    play(scenario, StreamServer(hold_credit=True))


def test_a_clean_exit_is_bounded_by_the_sessions_close_timeout_and_then_cancels_the_call() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        with pytest.raises(speech.SessionTimeoutError, match="end of the session") as caught:
            async with session:
                await session.send_audio(FRAME)
        assert caught.value.admitted is True
        assert fake.half_closed
        await fake.until(lambda: fake.live == 0)
        with pytest.raises(speech.SessionTimeoutError):
            await session.close()

    play(scenario, StreamServer(hold_finals=True), close_timeout=0.05)


def test_the_bounds_given_with_the_session_are_the_ones_its_exits_use(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        bounds: list[float | None] = []
        within = session._within

        async def spy(work: Any, timeout: float | None) -> Any:
            bounds.append(timeout)
            return await within(work, timeout)

        monkeypatch.setattr(session, "_within", spy)
        async with session:
            pass
        assert bounds == [3.0, 11.0]

    play(scenario, open_timeout=3.0, close_timeout=11.0, cancel_timeout=7.0)

    async def abandoned(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        bounds: list[float | None] = []
        within = session._within

        async def spy(work: Any, timeout: float | None) -> Any:
            bounds.append(timeout)
            return await within(work, timeout)

        monkeypatch.setattr(session, "_within", spy)
        with pytest.raises(KeyError):
            async with session:
                raise KeyError("the caller's own failure")
        assert bounds == [3.0, 7.0]

    play(abandoned, open_timeout=3.0, close_timeout=11.0, cancel_timeout=7.0)


@pytest.mark.parametrize("use", ["finalize", "close"])
def test_a_timeout_runs_from_the_call_and_covers_the_wait_behind_another_writer(use: str) -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        sender = await held_writer(session)
        with pytest.raises(speech.SessionTimeoutError) as caught:
            await getattr(session, use)(timeout=0.05)
        assert caught.value.admitted is True
        with pytest.raises(speech.SessionTimeoutError):
            await sender

    play(scenario, deaf(), input_format=STEREO)


def test_a_sender_cancelled_while_the_session_is_cancelled_reads_as_cancelled() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        sender = await held_writer(session)
        cancelling = asyncio.create_task(session.cancel(timeout=0.05))
        await turns()
        sender.cancel()
        await asyncio.wait({sender})
        assert sender.cancelled()
        await cancelling
        with pytest.raises(speech.SessionClosedError):
            await session.send_audio(LARGE)

    play(scenario, deaf(), input_format=STEREO)


@pytest.mark.parametrize("turns_before_the_cancel", [1, 2, 3, 4])
def test_a_caller_cancelled_as_its_call_fails_leaves_asyncio_nothing_to_report(
    turns_before_the_cancel: int,
) -> None:
    async def fail() -> None:
        raise KeyError("the work's own failure")

    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            caller = asyncio.create_task(session._within(fail(), None))
            # On one of these turns the work has failed and its caller has not yet run again.
            for _ in range(turns_before_the_cancel):
                await asyncio.sleep(0)
            caller.cancel()
            await asyncio.wait({caller})
            assert caller.cancelled() or isinstance(caller.exception(), KeyError)

    # `run` fails on a task whose exception nobody took.
    play(scenario)


def test_a_caller_cancelled_once_its_call_has_failed_gets_its_cancellation_and_not_the_error() -> (
    None
):
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        outcomes = []
        async with session:
            for turns_before_the_cancel in (1, 2, 3, 4):
                failed: list[bool] = []

                async def fail(failed: list[bool] = failed) -> None:
                    failed.append(True)
                    raise KeyError("the work's own failure")

                caller = asyncio.create_task(session._within(fail(), None))
                for _ in range(turns_before_the_cancel):
                    await asyncio.sleep(0)
                caller.cancel()
                outcomes.append((bool(failed), caller))
                await asyncio.wait({caller})
        # Which turn it is belongs to asyncio; on one of them the work had failed already.
        assert (True, True) in [(failed, caller.cancelled()) for failed, caller in outcomes]

    play(scenario)


def test_a_sender_cancelled_in_a_write_leaves_the_session_open_and_its_frame_sent() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            sender = await held_writer(session)
            sender.cancel()
            await asyncio.wait({sender})
            assert sender.cancelled()
            fake.hear()
            await session.send_audio(LARGE)
            final = await session.finalize()
        assert final.end_sample_offset == 3 * len(LARGE) // STEREO.sample_width
        assert fake.bodies() == ["open", "audio", "audio", "audio", "finalize"]

    play(scenario, deaf(), input_format=STEREO)


def test_a_task_cancelled_in_a_clean_exit_abandons_the_session() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async def use() -> None:
            async with session:
                await session.send_audio(FRAME)

        user = asyncio.create_task(use())
        await fake.until(lambda: fake.half_closed)
        user.cancel()
        await asyncio.wait({user})
        assert user.cancelled()
        await fake.until(lambda: fake.live == 0)
        with pytest.raises(speech.SessionClosedError):
            await session.send_audio(FRAME)

    play(scenario, StreamServer(hold_finals=True))


def test_a_write_grpc_refuses_ends_the_session_with_a_typed_error_past_a_full_buffer(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def refuse(event: Any) -> None:
        raise grpc.aio.UsageError("synthetic")

    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        await session.open()
        await flood(fake, session)
        monkeypatch.setattr(session._call, "write", refuse)
        with pytest.raises(speech.SessionCancelledError) as caught:
            await session.send_audio(FRAME)
        assert caught.value.__cause__ is None
        assert caught.value.__suppress_context__
        events: list[Any] = []
        with pytest.raises(speech.SessionCancelledError):
            await collect(session, events)
        # The reader was let past the bound with the event it held.
        assert len(events) == 17

    play(scenario, StreamServer(output_stall_timeout_ms=NO_STALL_MS))


def test_send_audio_takes_bytes_and_contiguous_views_of_bytes() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        samples = array.array("h", range(320))
        frame = samples.tobytes()
        views = (memoryview(frame), memoryview(samples).cast("B"), memoryview(samples).cast("b"))
        async with session:
            for data in (frame, bytearray(frame), *views):
                await session.send_audio(data)
            assert (await session.finalize()).end_sample_offset == 5 * 320
        sent = [event.audio for event in fake.received if event.HasField("audio")]
        assert [audio.data for audio in sent] == [frame] * 5
        assert [audio.sample_count for audio in sent] == [320] * 5

    play(scenario)


def test_send_audio_refuses_what_is_not_a_contiguous_view_of_bytes() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        samples = array.array("h", range(320))
        refused: tuple[Any, ...] = (
            640,
            [0] * 640,
            "x" * 640,
            None,
            samples,
            memoryview(samples),
            memoryview(bytes(1280))[::2],
        )
        async with session:
            for data in refused:
                with pytest.raises(TypeError, match="audio is a"):
                    await session.send_audio(data)
            await barrier(fake, session)
            assert fake.bodies() == ["open", "ping"]
            await session.send_audio(bytearray(FRAME))
            assert (await session.finalize()).end_sample_offset == 320

    play(scenario)


def test_closing_the_client_ends_the_sessions_made_from_it() -> None:
    async def main() -> None:
        fake = StreamServer(heartbeat_interval_ms=10)
        async with fake.serving() as endpoint:
            async with speech.SpeechClient(endpoint) as stream:
                options: dict[str, Any] = {"language": "en", "input_format": PCM}
                finished = stream.transcribe(TARGET, **options)
                async with finished:
                    await finished.send_audio(FRAME)
                session = stream.transcribe(TARGET, **options)
                unopened = stream.transcribe(TARGET, **options)
                await session.open()
                await fake.until(lambda: "ping" in fake.bodies())
                waiting = asyncio.create_task(taken(session))
            await fake.until(lambda: fake.live == 0)
            assert session._pinger is not None
            assert session._pinger.done()
            assert await waiting == []
            for closed in (session, unopened):
                calls = (closed.open(), closed.send_audio(FRAME), closed.finalize(), closed.close())
                for call in calls:
                    with pytest.raises(speech.SessionClosedError):
                        await call
                with pytest.raises(speech.SessionClosedError):
                    async with closed:
                        pass
                await closed.cancel()
                assert await taken(closed) == []
            with pytest.raises(speech.SessionClosedError):
                stream.transcribe(TARGET, **options)
            await stream.aclose()
            # A session that had ended keeps its events and its totals.
            assert len(await taken(finished)) == 2
            assert await finished.close() == speech.SessionTotals(320, 0, 0, 1, 0)

    run(main)


def test_closing_the_client_waits_for_a_session_no_longer_than_its_cancel_timeout(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    async def main() -> None:
        async with StreamServer().serving() as endpoint:
            stream = speech.SpeechClient(endpoint)
            session = stream.transcribe(
                TARGET, language="en", input_format=PCM, cancel_timeout=0.05
            )
            await session.open()
            monkeypatch.setattr(session, "_until_ended", lambda: asyncio.Event().wait())
            await stream.aclose()

    run(main)


def test_a_channel_the_caller_passes_is_used_and_left_open() -> None:
    async def main() -> None:
        fake = StreamServer()
        options: dict[str, Any] = {"language": "en", "input_format": PCM}
        async with fake.serving() as endpoint:
            channel = grpc.aio.insecure_channel(endpoint, options=client.CHANNEL_OPTIONS)
            for _ in range(2):
                stream = speech.SpeechClient(channel=channel)
                async with stream, stream.transcribe(TARGET, **options) as session:
                    await session.send_audio(FRAME)
            assert fake.calls == 2
            # Closing the client ends its sessions before it returns, open or not.
            stream = speech.SpeechClient(channel=channel)
            session, unopened = (stream.transcribe(TARGET, **options) for _ in range(2))
            await session.open()
            await stream.aclose()
            assert session._ended
            with pytest.raises(speech.SessionClosedError):
                await unopened.open()
            assert unopened._call is None
            # A channel its owner closed: no error of gRPC's reaches the caller.
            session = speech.SpeechClient(channel=channel).transcribe(TARGET, **options)
            await channel.close()
            for call in (session.open(), session.open(), session.send_audio(FRAME)):
                with pytest.raises(speech.SessionClosedError):
                    await call
            await session.cancel()
            assert await taken(session) == []

    run(main)
    both: dict[str, Any] = {"endpoint": "127.0.0.1:1", "channel": object()}
    for arguments in ({}, both):
        with pytest.raises(ValueError, match="an endpoint or a channel"):
            speech.SpeechClient(**arguments)


def test_a_closed_session_raises_the_closed_session_error_and_repeats_its_totals() -> None:
    async def scenario(fake: StreamServer, session: speech.SpeechToTextSession) -> None:
        async with session:
            await session.send_audio(FRAME)
        for call in (session.open(), session.send_audio(FRAME), session.finalize()):
            with pytest.raises(speech.SessionClosedError):
                await call
        assert await session.close() == speech.SessionTotals(320, 0, 0, 1, 0)
        await session.cancel()
        assert len(await taken(session)) == 2

    play(scenario)


@pytest.mark.parametrize(
    ("status", "error"),
    [
        (grpc.StatusCode.UNIMPLEMENTED, speech.PreconditionFailedError),
        (grpc.StatusCode.PERMISSION_DENIED, speech.ServerInternalError),
        (grpc.StatusCode.UNAUTHENTICATED, speech.ServerInternalError),
        (grpc.StatusCode.INTERNAL, speech.ServerInternalError),
    ],
)
def test_a_status_without_a_class_of_its_own_lands_where_the_errors_say(
    status: grpc.StatusCode, error: type[speech.SpeechError]
) -> None:
    failure = _session.failure_of(status, None, None)

    assert type(failure) is error
    assert vars(failure) == {
        "reason": None,
        "code": None,
        "grpc_status": status.name,
        "admitted": None,
    }
    assert not isinstance(failure, (TransportError, RequestTimeoutError))


def test_a_session_is_obtained_from_the_client_and_not_constructed() -> None:
    with pytest.raises(TypeError, match="obtained from a SpeechClient"):
        speech.SpeechToTextSession()
    assert list(inspect.signature(speech.SpeechToTextSession).parameters) == []


_FOREIGN = ("grpc", "google", "tensorplate.stream")


def _foreign(hint: Any) -> bool:
    """Whether a type hint or a default names, at any depth, a gRPC or a generated type."""
    module = getattr(hint, "__module__", None) or ""
    named = any(module == root or module.startswith(root + ".") for root in _FOREIGN)
    return named or any(_foreign(inner) for inner in typing.get_args(hint))


def _surface(exported: type) -> Iterator[tuple[str, Any]]:
    """The functions a caller reaches on `exported`: public ones, inherited ones and dunders."""
    for name, member in inspect.getmembers(exported):
        if name.startswith("_") and not name.endswith("__"):
            continue
        function = member.fget if isinstance(member, property) else member
        if inspect.isfunction(function) and function.__module__.startswith("tensorplate."):
            yield name, function


def test_the_public_names_are_the_sdks_own_types() -> None:
    assert not _foreign(speech.SpeechToTextEvent)
    assert not _foreign(speech.TextToSpeechEvent)
    assert _foreign(pb.Open | None)
    assert _foreign(type(grpc.StatusCode.OK))
    for name in speech.__all__:
        exported = getattr(speech, name)
        if not isinstance(exported, type):
            continue
        assert exported.__module__.startswith("tensorplate.speech."), name
        named = dict(typing.get_type_hints(exported))
        for member, function in _surface(exported):
            for part, hint in typing.get_type_hints(function).items():
                named[f"{member}({part})"] = hint
            # No private name in what a caller reads: an annotation or a default's repr.
            assert not re.search(r"\b_[A-Za-z]", str(inspect.signature(function))), member
            for parameter in inspect.signature(function).parameters.values():
                if parameter.default is not inspect.Parameter.empty:
                    named[f"{member}({parameter.name}=)"] = type(parameter.default)
        assert [where for where, hint in named.items() if _foreign(hint)] == [], name
    assert issubclass(speech.SpeechError, TensorPlateError)

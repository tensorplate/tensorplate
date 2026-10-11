"""The in-process stream server of ``stream_server.py``, held to the recorded frames.

The frames under ``test/contract/fixtures/stream/v1`` are the ones the C++
bindings are tested against. These tests drive the server with the generated
stub alone, so they say nothing about the SDK's client.
"""

from __future__ import annotations

import asyncio
import contextlib
import os
from typing import Any

import pytest

from tensorplate.errors import MissingDependencyError

try:
    import tensorplate.stream  # noqa: F401
except MissingDependencyError as missing:
    if os.environ.get("TENSORPLATE_REQUIRE_SPEECH_EXTRAS") == "1":
        raise
    pytest.skip(str(missing), allow_module_level=True)

import grpc
import grpc.aio

from stream_server import STATUS, StreamServer, recorded
from tensorplate.stream.v1 import session_pb2 as pb
from tensorplate.stream.v1 import session_pb2_grpc as rpc

_STT_CLIENT = ["stt_open", *(f"stt_audio_{n}" for n in range(1, 6)), "stt_finalize_1"]
_STT_CLIENT += ["stt_audio_6", "stt_finalize_2", "stt_audio_7", "stt_audio_8"]


def exchange(
    server: StreamServer, events: list[Any], *, half_close: bool = True, then: str = ""
) -> tuple[list[Any], Any]:
    """Send `events` on one call and return what the server sent and the final status.

    `then` is a reason the server ends the session with once it is ready.
    """

    async def call_once() -> tuple[list[Any], Any]:
        async with server.serving() as endpoint, grpc.aio.insecure_channel(endpoint) as channel:
            call = rpc.SessionServiceStub(channel).Session()
            got: list[Any] = []
            with contextlib.suppress(grpc.aio.AioRpcError):
                for event in events:
                    await call.write(event)
                if then:
                    got.append(await call.read())
                    server.end(then)
                if half_close:
                    await call.done_writing()
                while (event := await call.read()) is not grpc.aio.EOF:
                    got.append(event)
            return got, await call.code()

    return asyncio.run(asyncio.wait_for(call_once(), 60))


def test_the_recorded_speech_to_text_session_is_served_as_recorded() -> None:
    server = StreamServer()

    got, status = exchange(server, [recorded(name) for name in _STT_CLIENT])

    assert status is grpc.StatusCode.OK
    assert got[0].SerializeToString() == recorded("stt_ready").SerializeToString()
    assert [event.sequence for event in got] == list(range(1, len(got) + 1))
    finals = [event.final_transcript for event in got if event.HasField("final_transcript")]
    answers = [recorded("stt_final_2").final_transcript, recorded("stt_final_3").final_transcript]
    assert [(final.finalize_sequence, final.end_sample_offset) for final in finals] == [
        *((answer.finalize_sequence, answer.end_sample_offset) for answer in answers),
        (0, 1280),
    ]
    endpoints = [event.endpoint_detected for event in got if event.HasField("endpoint_detected")]
    last = recorded("stt_endpoint_4").endpoint_detected
    assert (endpoints[-1].reason, endpoints[-1].end_sample_offset) == (
        last.reason,
        last.end_sample_offset,
    )
    closed, expected = got[-1].session_closed, recorded("stt_closed").session_closed
    assert not closed.HasField("cause")
    assert (closed.state, closed.admission) == (expected.state, expected.admission)
    assert closed.last_accepted_sequence == expected.last_accepted_sequence
    assert closed.totals.input_audio_samples == expected.totals.input_audio_samples


def _open(**changes: Any) -> Any:
    opened = recorded("stt_open")
    for name, value in changes.items():
        if name in ("deployment_id", "generation"):
            setattr(opened.open.resolved, name, value)
        elif name == "sequence":
            opened.sequence = value
        else:
            setattr(opened.open, name, value)
    return opened


@pytest.mark.parametrize(
    ("reason", "opened", "server"),
    [
        ("invalid_event", _open(sequence=2), StreamServer()),
        ("backend_unsupported_capability", _open(language="xx"), StreamServer()),
        ("target_unresolved", recorded("open_selector"), StreamServer()),
        ("target_mismatch", _open(deployment_id="another-deployment"), StreamServer()),
        ("stale_generation", _open(generation=6), StreamServer()),
        ("admission_closed", _open(), StreamServer(admission_open=False)),
        ("session_count_limit", _open(), StreamServer(max_sessions=0)),
    ],
)
def test_an_open_is_refused_with_the_recorded_refusal_and_no_event(
    reason: str, opened: Any, server: StreamServer
) -> None:
    got, status = exchange(server, [opened])

    assert got == []
    assert status is STATUS[reason]
    assert server.refusals == [recorded(f"refused_{reason}")]
    assert server.closed == []


def _audio(sequence: int, offset: int, samples: int = 160, data: bytes | None = None) -> Any:
    audio = pb.Audio(data=data or bytes(samples), sample_count=samples, sample_offset=offset)
    return pb.ClientEvent(sequence=sequence, session_id=_SESSION, audio=audio)


_SESSION = "synthetic-session-1"
_SECOND_OPEN = pb.ClientEvent(sequence=2, session_id=_SESSION, open=recorded("stt_open").open)


def _ends(got: list[Any], name: str) -> None:
    closed, expected = got[-1].session_closed, recorded(name).session_closed
    assert (closed.state, closed.cause, closed.admission) == (
        expected.state,
        expected.cause,
        expected.admission,
    )


def test_input_above_the_credit_fails_the_session_as_recorded() -> None:
    server = StreamServer(hold_credit=True, max_frame_ms=320)
    # 8,000 bytes of credit: three frames of 2,560 fit and the fourth does not.
    frames = [_audio(2 + n, 2560 * n, 2560) for n in range(4)]

    got, status = exchange(server, [recorded("stt_open"), *frames], half_close=False)

    assert status is grpc.StatusCode.RESOURCE_EXHAUSTED
    assert [event.accepted.input_credit.bytes.used for event in got[1:-1]] == [2560, 5120, 7680]
    _ends(got, "failed_input_credit_exceeded")
    assert got[-1].session_closed.last_accepted_sequence == 4


@pytest.mark.parametrize(
    ("what", "event"),
    [
        ("a gap in the sequence", _audio(3, 0)),
        ("another session's id", pb.ClientEvent(sequence=2, session_id="other", ping=pb.Ping())),
        ("an offset that is not the samples sent", _audio(2, 160)),
        ("a sample count that is not the data's", _audio(2, 0, 160, data=bytes(80))),
        ("a frame above the bound", _audio(2, 0, 400)),
        ("a second Open", _SECOND_OPEN),
        ("a body the schema lacks", recorded("added_client_body")),
        ("text in a speech-to-text session", recorded("tts_text_1")),
    ],
)
def test_an_event_the_schema_does_not_allow_fails_the_session(what: str, event: Any) -> None:
    got, status = exchange(StreamServer(), [recorded("stt_open"), event], half_close=False)

    assert status is grpc.StatusCode.INVALID_ARGUMENT, what
    closed = got[-1].session_closed
    assert closed.state == pb.SESSION_STATE_FAILED
    assert closed.cause == recorded("refused_invalid_event").cause


def test_audio_after_a_short_frame_fails_the_session_unless_a_finalize_came_between() -> None:
    short, finalize = _audio(2, 0, 80), recorded("stt_finalize_1")
    finalize.sequence = 3

    got, status = exchange(StreamServer(), [recorded("stt_open"), short, _audio(3, 80)])
    assert status is grpc.StatusCode.INVALID_ARGUMENT

    got, status = exchange(StreamServer(), [recorded("stt_open"), short, finalize, _audio(4, 80)])
    assert status is grpc.StatusCode.OK
    assert got[-1].session_closed.totals.utterances_completed == 2


def test_a_cancel_a_ping_and_a_timer_end_as_recorded() -> None:
    cancel = recorded("cancel")
    cancel.sequence = 3
    ping = recorded("ping")
    ping.sequence = 2

    got, status = exchange(StreamServer(), [recorded("stt_open"), ping, cancel], half_close=False)
    assert status is grpc.StatusCode.CANCELLED
    assert [event.WhichOneof("body") for event in got] == [
        "ready",
        "pong",
        "cancel_accepted",
        "session_closed",
    ]
    _ends(got, "closed_client_cancelled")

    got, status = exchange(
        StreamServer(), [recorded("stt_open")], half_close=False, then="idle_timeout"
    )
    assert status is grpc.StatusCode.DEADLINE_EXCEEDED
    _ends(got, "closed_idle_timeout")

"""The in-process stream server of ``stream_server.py``, held to the recorded frames.

The frames under ``test/contract/fixtures/stream/v1`` are the ones the C++
bindings are tested against. These tests drive the server with the generated
stub alone, so they say nothing about the SDK's client.
"""

from __future__ import annotations

import asyncio
import contextlib
import os
from collections.abc import Callable
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

from stream_server import STATUS, StreamServer, end_cause, frames, recorded
from tensorplate.stream.v1 import session_pb2 as pb
from tensorplate.stream.v1 import session_pb2_grpc as rpc

Steps = list[tuple[Any, ...]]


def converse(
    server: StreamServer, *turns: tuple[list[Any], Steps], half_close: bool = True
) -> tuple[list[Any], Any]:
    """Run one call and return what the server sent and the final status.

    Each turn is client events to send and the steps to queue on the server
    once it has taken them; the last turn's events end with the half-close.
    """

    async def call_once() -> tuple[list[Any], Any]:
        async with server.serving() as endpoint, grpc.aio.insecure_channel(endpoint) as channel:
            call = rpc.SessionServiceStub(channel).Session()
            got: list[Any] = []
            sent = 0
            with contextlib.suppress(grpc.aio.AioRpcError):
                for events, steps in turns:
                    for event in events:
                        await call.write(event)
                    sent += len(events)
                    last = half_close and steps is turns[-1][1]
                    if last:
                        await call.done_writing()
                    if steps:
                        await server.until(_took(server, sent, last))
                    for step, *arguments in steps:
                        getattr(server, step)(*arguments)
                while (event := await call.read()) is not grpc.aio.EOF:
                    got.append(event)
            return got, await call.code()

    return asyncio.run(asyncio.wait_for(call_once(), 60))


def _took(server: StreamServer, count: int, half_closed: bool) -> Callable[[], bool]:
    return lambda: len(server.received) == count and server.half_closed == half_closed


def exchange(
    server: StreamServer, events: list[Any], *, half_close: bool = True, then: str = ""
) -> tuple[list[Any], Any]:
    """Send `events` on one call; `then` is a reason the server ends the session with after them."""
    return converse(
        server, (events, [("end", then)] if then else []), half_close=half_close and not then
    )


def test_the_recorded_speech_to_text_session_is_served_frame_for_frame() -> None:
    # The server has no recognizer, so this supplies what one decides and nothing else: each
    # transcript's text and intervals, the silence that ends the first utterance at sample
    # 480, and when audio is consumed and an Accepted goes out. Every other byte of the
    # twenty recorded server frames is the server's own, and none differs.
    server = StreamServer(hold_credit=True, hold_accepted=True, hold_finals=True)
    first = [(0, 30000, "synthetic"), (30000, 60000, " transcript")]
    second = [(75000, 100000, "synthetic"), (100000, 110000, " words")]
    turns: list[tuple[list[str], Steps]] = [
        (["stt_open"], []),
        (["stt_audio_1", "stt_audio_2"], [("accept",), ("partial", "synthetic")]),
        (
            ["stt_audio_3"],
            [
                ("consume", 320),
                ("accept",),
                ("partial", "synthetic transcript"),
                ("endpoint", 480),
                ("final", first),
                ("consume", 320),
                ("accept",),
            ],
        ),
        (
            ["stt_audio_4", "stt_audio_5", "stt_finalize_1"],
            [
                ("accept",),
                ("partial", "synthetic words"),
                ("endpoint",),
                ("final", second),
                ("consume", 240),
            ],
        ),
        (
            ["stt_audio_6", "stt_finalize_2"],
            [("accept",), ("endpoint",), ("final", []), ("consume", 160)],
        ),
        (
            ["stt_audio_7", "stt_audio_8"],
            [
                ("accept",),
                ("partial", "synthetic end"),
                ("endpoint",),
                ("final", [(135000, 160000, "synthetic end")]),
            ],
        ),
    ]
    session = frames("stt_session")
    client = [name for name, kind, _ in session if kind == "ClientEvent"]
    assert [name for names, _ in turns for name in names] == client

    got, status = converse(
        server, *(([recorded(name) for name in names], steps) for names, steps in turns)
    )

    assert status is grpc.StatusCode.OK
    expected = [(name, encoded) for name, kind, encoded in session if kind == "ServerEvent"]
    assert got == [recorded(name) for name, _ in expected]
    assert [event.SerializeToString() for event in got] == [encoded for _, encoded in expected]


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


def _first(event: Any) -> Any:
    """`event` as the first of a call: sequence one and no session named."""
    event.sequence = 1
    event.ClearField("session_id")
    return event


@pytest.mark.parametrize(
    ("reason", "opened", "server"),
    [
        ("invalid_event", _open(sequence=2), StreamServer()),
        ("invalid_event", _first(recorded("stt_audio_1")), StreamServer()),
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


def test_a_short_frame_stays_the_last_across_an_automatic_endpoint() -> None:
    server = StreamServer(hold_finals=True)
    opening = ([recorded("stt_open"), _audio(2, 0, 80)], [("endpoint",), ("final",)])

    got, status = converse(server, opening, ([_audio(3, 80)], []), half_close=False)

    assert status is grpc.StatusCode.INVALID_ARGUMENT
    assert [event.WhichOneof("body") for event in got[2:]] == [
        "endpoint_detected",
        "final_transcript",
        "session_closed",
    ]


def _finalize(sequence: int) -> Any:
    return pb.ClientEvent(sequence=sequence, session_id=_SESSION, finalize=pb.Finalize())


def test_audio_before_the_answer_to_a_finalize_fails_the_session() -> None:
    server = StreamServer(hold_finals=True)
    events = [recorded("stt_open"), _audio(2, 0), _finalize(3), _audio(4, 160)]

    got, status = exchange(server, events, half_close=False)

    assert status is grpc.StatusCode.FAILED_PRECONDITION
    closed = got[-1].session_closed
    assert (closed.state, closed.cause) == (
        pb.SESSION_STATE_FAILED,
        end_cause("illegal_transition"),
    )
    assert closed.last_accepted_sequence == 2
    assert not any(event.HasField("final_transcript") for event in got)


def test_a_finalize_repeated_before_its_answer_has_no_answer_of_its_own() -> None:
    server = StreamServer(hold_finals=True)
    events = [recorded("stt_open"), _audio(2, 0), _finalize(3), _finalize(4)]

    got, status = converse(server, (events, [("final",)]))

    assert status is grpc.StatusCode.OK
    finals = [event.final_transcript for event in got if event.HasField("final_transcript")]
    assert [(final.utterance_id, final.finalize_sequence) for final in finals] == [(1, 3)]


def test_an_automatic_endpoints_final_names_no_finalize_and_precedes_a_finalizes_own() -> None:
    server = StreamServer(hold_finals=True)
    opening = ([recorded("stt_open"), _audio(2, 0)], [("endpoint", 100)])
    joined = ([_audio(3, 160), _finalize(4)], [("final",), ("final",)])

    got, status = converse(server, opening, joined)

    assert status is grpc.StatusCode.OK
    silence, client = pb.ENDPOINT_REASON_SILENCE, pb.ENDPOINT_REASON_CLIENT_FINALIZE
    task = [event for event in got if event.WhichOneof("body") not in ("ready", "accepted")]
    assert [event.WhichOneof("body") for event in task] == [
        "endpoint_detected",
        "final_transcript",
        "endpoint_detected",
        "final_transcript",
        "session_closed",
    ]
    endpoints = [task[0].endpoint_detected, task[2].endpoint_detected]
    assert [(e.utterance_id, e.end_sample_offset, e.reason) for e in endpoints] == [
        (1, 100, silence),
        (2, 320, client),
    ]
    finals = [task[1].final_transcript, task[3].final_transcript]
    assert [(f.utterance_id, f.end_sample_offset, f.finalize_sequence) for f in finals] == [
        (1, 100, 0),
        (2, 320, 4),
    ]
    assert task[4].session_closed.totals.utterances_completed == 2


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

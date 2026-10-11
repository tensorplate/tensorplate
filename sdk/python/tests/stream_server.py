"""An in-process stream server for the streaming client's tests.

It serves speech-to-text sessions as the stream schema and
``docs/architecture/serving-worker.md`` ("Logical sessions", "Session
dispatch") say a worker does, and refuses what they say it refuses. ``test_streaming_server.py`` holds it to the recorded frames
under ``test/contract/fixtures/stream/v1``. It runs no timer, detects no
endpoint by itself and serves no text-to-speech session; a test ends a
session from the server's side with ``end()``. A `fault` makes it break one
rule, for the tests of what the client does then.
"""

from __future__ import annotations

import asyncio
import contextlib
import re
from collections.abc import AsyncIterator
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import grpc
import grpc.aio

from tensorplate.stream.v1 import session_pb2 as pb
from tensorplate.stream.v1 import session_pb2_grpc as rpc

REPOSITORY = Path(__file__).resolve().parents[3]
FRAMES = REPOSITORY / "test/contract/fixtures/stream/v1"
_REASONS = REPOSITORY / "docs/observability/failure-reasons.md"

Code = grpc.StatusCode
# The status a reason ends a call with, by the kind of reason: invalid input, a
# precondition, a bound, a timer, unavailability, the client's own cancel.
STATUS = {
    "invalid_event": Code.INVALID_ARGUMENT,
    "empty_input": Code.INVALID_ARGUMENT,
    "backend_unsupported_capability": Code.FAILED_PRECONDITION,
    "target_unresolved": Code.FAILED_PRECONDITION,
    "target_mismatch": Code.FAILED_PRECONDITION,
    "stale_generation": Code.FAILED_PRECONDITION,
    "session_count_limit": Code.RESOURCE_EXHAUSTED,
    "input_credit_exceeded": Code.RESOURCE_EXHAUSTED,
    "idle_timeout": Code.DEADLINE_EXCEEDED,
    "admission_closed": Code.UNAVAILABLE,
    "worker_shutdown": Code.UNAVAILABLE,
    "backend_reset": Code.UNAVAILABLE,
    "client_cancelled": Code.CANCELLED,
}
# Reasons that end a session failed; every other one ends it closed (serving-worker.md,
# "How sessions end").
FAILS = ("invalid_event", "empty_input", "input_credit_exceeded", "backend_reset")
_SAMPLE_BYTES = {pb.AUDIO_ENCODING_PCM_S16LE: 2, pb.AUDIO_ENCODING_MULAW: 1}


def recorded(name: str) -> Any:
    """The recorded frame called `name`, decoded with the SDK's generated modules."""
    for path in sorted(FRAMES.glob("*.frames")):
        for line in path.read_text(encoding="utf-8").splitlines():
            frame = line.split("\t")
            if frame[0] == name:
                return getattr(pb, frame[1].rsplit(".", 1)[1]).FromString(bytes.fromhex(frame[2]))
    raise AssertionError(f"no recorded frame {name}")


def end_cause(reason: str) -> Any:
    """`reason` with the error code the failure reason taxonomy pairs with it."""
    row = re.search(rf"^\| `{reason}`.*\| `(\w+)`\s*\|$", _REASONS.read_text("utf-8"), re.M)
    assert row, f"{reason} is not in the failure reason taxonomy"
    return pb.EndCause(
        reason=pb.FailureReason.Value(f"FAILURE_REASON_{reason.upper()}"),
        code=pb.ErrorCode.Value(f"ERROR_CODE_{row.group(1).upper()}"),
    )


class _Ends(Exception):
    """The session ends with the reason given."""


@dataclass
class StreamServer:
    """One worker's stream listener: what it serves, what it was sent, and its faults."""

    deployment_id: str = "synthetic-deployment"
    generation: int = 7
    descriptor_digest: str = "sha256:" + "0" * 64
    languages: tuple[str, ...] = ("en",)
    formats: tuple[tuple[int, int, int], ...] = (
        (pb.AUDIO_ENCODING_MULAW, 8000, 1),
        (pb.AUDIO_ENCODING_PCM_S16LE, 16000, 1),
    )
    min_frame_ms: int = 20
    max_frame_ms: int = 40
    heartbeat_interval_ms: int = 10000
    max_sessions: int = 1
    admission_open: bool = True
    # Accepted audio stays charged until `consume()` or the utterance's Finalize.
    hold_credit: bool = False
    partials: bool = False
    fault: str = ""
    calls: int = 0
    live: int = 0
    received: list[Any] = field(default_factory=list)
    refusals: list[Any] = field(default_factory=list)
    closed: list[Any] = field(default_factory=list)
    _inbox: asyncio.Queue[tuple[str, Any]] | None = None

    @contextlib.asynccontextmanager
    async def serving(self) -> AsyncIterator[str]:
        # A fixed HTTP/2 window, so that a server that reads nothing holds a writer back.
        server = grpc.aio.server(options=[("grpc.http2.bdp_probe", 0)])
        rpc.add_SessionServiceServicer_to_server(self, server)
        port = server.add_insecure_port("127.0.0.1:0")
        await server.start()
        try:
            yield f"127.0.0.1:{port}"
        finally:
            await server.stop(None)

    def bodies(self) -> list[str | None]:
        return [event.WhichOneof("body") for event in self.received]

    def consume(self) -> None:
        """Take the audio the live session holds, which returns its credit."""
        assert self._inbox is not None
        self._inbox.put_nowait(("consume", None))

    def end(self, reason: str) -> None:
        """End the live session from the server's side."""
        assert self._inbox is not None
        self._inbox.put_nowait(("end", reason))

    def _admit(self, first: Any) -> Any:
        """The Ready for an Open, or the reason it is refused with."""
        opened = first.open
        if first.WhichOneof("body") != "open" or first.sequence != 1 or first.session_id:
            raise _Ends("invalid_event")
        if opened.WhichOneof("target") == "selector":
            raise _Ends("target_unresolved")
        target = opened.resolved
        named = re.fullmatch(r"[A-Za-z0-9._-]{1,128}", target.deployment_id)
        if not named or target.deployment_id in (".", ".."):
            raise _Ends("invalid_event")
        if not re.fullmatch(r"[ -~]{1,64}", opened.language):
            raise _Ends("invalid_event")
        served = (self.deployment_id, self.descriptor_digest)
        if (target.deployment_id, target.descriptor_digest) != served:
            raise _Ends("target_mismatch")
        if target.generation != self.generation:
            raise _Ends("stale_generation")
        audio = opened.input_format
        declared = (audio.encoding, audio.sample_rate_hz, audio.channels) in self.formats
        if (
            opened.mode != pb.SESSION_MODE_STT_STREAMING
            or opened.language not in self.languages
            or not declared
        ):
            raise _Ends("backend_unsupported_capability")
        if not self.admission_open:
            raise _Ends("admission_closed")
        if self.live >= self.max_sessions:
            raise _Ends("session_count_limit")
        second = audio.sample_rate_hz * audio.channels * _SAMPLE_BYTES[audio.encoding]
        return pb.Ready(
            session_id=f"synthetic-session-{self.calls}",
            generation=self.generation,
            descriptor_digest=self.descriptor_digest,
            limits=pb.SessionLimits(
                idle_timeout_ms=60000,
                heartbeat_interval_ms=self.heartbeat_interval_ms,
                liveness_timeout_ms=30000,
                max_duration_ms=3600000,
                finalize_deadline_ms=10000,
                output_stall_timeout_ms=5000,
                speech_to_text=pb.SpeechToTextLimits(
                    min_frame_ms=self.min_frame_ms,
                    max_frame_ms=self.max_frame_ms,
                    max_utterance_ms=30000,
                ),
            ),
            remaining_duration_ms=3600000,
            input_format=audio,
            status=pb.SessionStatus(
                state=pb.SESSION_STATE_ACTIVE,
                input_credit=pb.InputCredit(bytes=pb.Usage(limit=second)),
                output_metadata_bytes=pb.Usage(limit=16384),
            ),
        )

    async def Session(self, unused_requests: Any, context: Any) -> None:
        self.calls += 1
        try:
            first = await context.read()
            if first is grpc.aio.EOF:
                return
            self.received.append(first)
            ready = self._admit(first)
        except _Ends as refusal:
            reason = str(refusal)
            refused = pb.OpenRefused(cause=end_cause(reason), admission=pb.ADMISSION_NOT_ADMITTED)
            self.refusals.append(refused)
            await context.abort(STATUS[reason], reason)
        self.live += 1
        try:
            await _Live(self, context, ready).run()
        finally:
            self.live -= 1


class _Live:
    """One admitted session."""

    def __init__(self, server: StreamServer, context: Any, ready: Any) -> None:
        self.server, self.context, self.ready = server, context, ready
        audio = ready.input_format
        self.rate, self.width = audio.sample_rate_hz, _SAMPLE_BYTES[audio.encoding] * audio.channels
        self.limit = ready.status.input_credit.bytes.limit
        self.out = self.used = self.samples = self.last = self.finals = 0
        self.utterance, self.utterance_start = 1, 0
        self.short = False
        self.inbox: asyncio.Queue[tuple[str, Any]] = asyncio.Queue()
        server._inbox = self.inbox

    async def emit(self, **body: Any) -> None:
        self.out += 1
        await self.context.write(pb.ServerEvent(sequence=self.out, **body))

    async def accepted(self) -> None:
        credit = pb.InputCredit(bytes=pb.Usage(used=self.used, limit=self.limit))
        await self.emit(accepted=pb.Accepted(accepted_sequence=self.last, input_credit=credit))

    async def pump(self) -> None:
        while (event := await self.context.read()) is not grpc.aio.EOF:
            self.inbox.put_nowait(("event", event))
        self.inbox.put_nowait(("half_close", None))

    async def run(self) -> None:
        fault = self.server.fault
        if fault == "silent":
            await asyncio.Event().wait()
        if fault == "event_before_ready":
            await self.emit(pong=pb.Pong())
        if fault == "sequence_gap":
            self.out += 1
        if fault == "other_generation":
            self.ready.generation += 1
        if fault == "no_credit":
            self.ready.status.ClearField("input_credit")
        await self.emit(ready=self.ready)
        if fault == "deaf":
            await asyncio.Event().wait()
        if fault == "drop":
            await self.context.abort(Code.UNAVAILABLE, "synthetic connection loss")
        if fault == "unknown_event":
            self.out += 1
            unknown = recorded("added_server_body")
            unknown.sequence = self.out
            await self.context.write(unknown)
        pump = asyncio.create_task(self.pump())
        try:
            expected = 2
            while True:
                kind, value = await self.inbox.get()
                if kind == "event":
                    self.server.received.append(value)
                    # Only a Cancel sent before the client read Ready may name no session.
                    early = value.WhichOneof("body") == "cancel" and not value.session_id
                    named = early or value.session_id == self.ready.session_id
                    if value.sequence != expected or not named:
                        raise _Ends("invalid_event")
                    expected += 1
                    await self.take(value)
                elif kind == "consume":
                    self.used = 0
                    await self.accepted()
                elif kind == "end":
                    raise _Ends(value)
                else:
                    if self.samples > self.utterance_start:
                        await self.transcript(pb.ENDPOINT_REASON_HALF_CLOSE, 0)
                    if fault != "ok_without_closed":
                        await self.close(None)
                    return
        except _Ends as end:
            await self.close(str(end))
        finally:
            pump.cancel()

    async def close(self, reason: str | None) -> None:
        closed = pb.SessionClosed(
            state=pb.SESSION_STATE_FAILED if reason in FAILS else pb.SESSION_STATE_CLOSED,
            admission=pb.ADMISSION_ADMITTED,
            last_accepted_sequence=self.last,
            totals=pb.SessionTotals(
                input_audio_samples=self.samples, utterances_completed=self.finals
            ),
        )
        if reason is not None:
            closed.cause.CopyFrom(end_cause(reason))
        self.server.closed.append(closed)
        await self.emit(session_closed=closed)
        if reason is not None and self.server.fault != "ok_with_cause":
            await self.context.abort(STATUS[reason], reason)

    async def take(self, event: Any) -> None:
        body = event.WhichOneof("body")
        if body == "ping":
            await self.emit(pong=pb.Pong())
        elif body == "status_request":
            await self.emit(status=pb.SessionStatus(state=pb.SESSION_STATE_ACTIVE))
        elif body == "cancel":
            await self.emit(cancel_accepted=pb.CancelAccepted())
            raise _Ends("client_cancelled")
        elif body == "finalize":
            if self.server.fault == "stall_finalize":
                return
            if self.used:
                self.used = 0
                await self.accepted()
            await self.transcript(pb.ENDPOINT_REASON_CLIENT_FINALIZE, event.sequence)
        elif body == "audio":
            await self.audio(event)
        else:
            # A second Open, a body the schema lacks, or input of another mode.
            raise _Ends("invalid_event")

    async def audio(self, event: Any) -> None:
        audio, limits = event.audio, self.ready.limits.speech_to_text
        count = audio.sample_count
        if not audio.data:
            raise _Ends("empty_input")
        if (
            count * self.width != len(audio.data)
            or audio.sample_offset != self.samples
            or count * 1000 > limits.max_frame_ms * self.rate
            or self.short
        ):
            raise _Ends("invalid_event")
        if self.used + len(audio.data) > self.limit:
            raise _Ends("input_credit_exceeded")
        self.short = count * 1000 < limits.min_frame_ms * self.rate
        self.samples, self.last = self.samples + count, event.sequence
        self.used += len(audio.data)
        if not self.server.hold_credit:
            self.used = 0
        await self.accepted()
        if self.server.partials and self.samples - count == self.utterance_start:
            partial = pb.PartialTranscript(
                utterance_id=self.utterance, revision=1, text="synthetic"
            )
            await self.emit(partial_transcript=partial)

    async def transcript(self, reason: Any, finalize_sequence: int) -> None:
        """End the open utterance: its endpoint, then its one final transcript."""
        fault, start, end = self.server.fault, self.utterance_start, self.samples
        endpoint = pb.EndpointDetected(
            utterance_id=self.utterance, end_sample_offset=end, reason=reason
        )
        segment = pb.TranscriptSegment(
            start_us=start * 1_000_000 // self.rate,
            end_us=-(-end * 1_000_000 // self.rate),
            text="synthetic transcript",
        )
        final = pb.FinalTranscript(
            utterance_id=self.utterance,
            segments=[segment] if end > start else [],
            end_sample_offset=end,
            finalize_sequence=finalize_sequence + (100 if fault == "other_finalize" else 0),
        )
        if fault != "final_without_endpoint":
            await self.emit(endpoint_detected=endpoint)
        if fault == "partial_after_endpoint":
            late = pb.PartialTranscript(utterance_id=self.utterance, revision=2, text="late")
            await self.emit(partial_transcript=late)
        await self.emit(final_transcript=final)
        self.finals += 1
        self.utterance, self.utterance_start, self.short = self.utterance + 1, end, False

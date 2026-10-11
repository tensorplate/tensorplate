"""One call of the stream schema's ``Session``: the exchange every kind of session shares."""

from __future__ import annotations

import asyncio
import contextlib
from collections.abc import Callable
from types import TracebackType
from typing import Any, Generic, TypeVar

import grpc
import grpc.aio

from tensorplate.errors import ErrorCode
from tensorplate.speech._buffer import EventBuffer, Signal
from tensorplate.speech.errors import (
    InvalidEventError,
    PreconditionFailedError,
    ResourceExhaustedError,
    ServerInternalError,
    ServiceUnavailableError,
    SessionCancelledError,
    SessionClosedError,
    SessionTimeoutError,
    SpeechError,
    StreamProtocolError,
)
from tensorplate.speech.events import SessionLimits, SessionTotals, SpeechToTextLimits, Target
from tensorplate.stream.v1 import session_pb2 as pb
from tensorplate.stream.v1.session_pb2_grpc import SessionServiceStub

EventT = TypeVar("EventT")
SessionT = TypeVar("SessionT", bound="Session[Any]")

# Transcripts and control events a session holds for its caller before it stops reading.
METADATA_BUFFER_BYTES = 16 * 1024

_ERROR_OF_STATUS: dict[str, type[SpeechError]] = {
    "INVALID_ARGUMENT": InvalidEventError,
    "FAILED_PRECONDITION": PreconditionFailedError,
    "RESOURCE_EXHAUSTED": ResourceExhaustedError,
    "DEADLINE_EXCEEDED": SessionTimeoutError,
    "UNAVAILABLE": ServiceUnavailableError,
    "CANCELLED": SessionCancelledError,
}


def enum_name(enum: object, value: int) -> str:
    """``ENDPOINT_REASON_SILENCE`` as ``silence``; a value this schema lacks as ``unspecified``."""
    names: Any = enum
    prefix = names.Name(0)[: -len("UNSPECIFIED")]
    try:
        return str(names.Name(value))[len(prefix) :].lower()
    except ValueError:
        return "unspecified"


def decode_refusal(trailing_metadata: object) -> pb.OpenRefused | None:
    """The ``OpenRefused`` that the status of a refused open carries, or ``None``.

    The stream schema has not fixed how the status carries it, so nothing is
    decoded yet and a refusal is known by its status code alone.
    """
    return None


def failure_of(
    status: grpc.StatusCode, cause: pb.EndCause | None, admitted: bool | None
) -> SpeechError:
    """The error for a call that ended with `status`; its class follows the status code alone."""
    reason = code = None
    if cause is not None:
        reason = enum_name(pb.FailureReason, cause.reason)
        with contextlib.suppress(ValueError):
            code = ErrorCode(enum_name(pb.ErrorCode, cause.code))
    kind = _ERROR_OF_STATUS.get(status.name, ServerInternalError)
    return kind(
        f"the session ended with status {status.name}" + (f" ({reason})" if reason else ""),
        reason=reason,
        code=code,
        grpc_status=status.name,
        admitted=admitted,
    )


class Session(Generic[EventT]):
    """What both kinds of session share. Obtained from a ``SpeechClient``, never constructed.

    Iterating a session yields its events in the order the server sent them
    and ends when the session does; a session that failed raises its error
    once the events read before the failure are taken.
    """

    def __init__(
        self, stub: SessionServiceStub, target: Target, open_timeout: float | None
    ) -> None:
        self._stub, self._target, self._open_timeout = stub, target, open_timeout
        self._call: Any = None
        self._reader: asyncio.Task[None] | None = None
        self._pinger: asyncio.Task[None] | None = None
        self._write_lock = asyncio.Lock()
        # Held by one input or one finalize at a time: input never overtakes a Finalize's answer.
        self._input_gate = asyncio.Lock()
        self._changed = Signal()
        self._events: EventBuffer[EventT] = EventBuffer(METADATA_BUFFER_BYTES)
        self._sent = 0
        self._received = 0
        self._session_id = ""
        self._ready: pb.Ready | None = None
        self._closed: pb.SessionClosed | None = None
        self._ended = False
        self._failure: SpeechError | None = None
        self._abort_error: SpeechError | None = None
        self._cancelling = False
        self._half_closed = False
        self._credit_used = self._credit_limit = 0
        self._unaccepted: dict[int, int] = {}
        self._last_input = self._accepted = 0
        self._finalizes: set[int] = set()
        self._answers: dict[int, EventT] = {}
        self._unknown_events = 0

    # --- what a subclass supplies ---------------------------------------------

    def _open_body(self, resolved: pb.ResolvedTarget) -> pb.Open:
        raise NotImplementedError

    def _check_ready(self, ready: pb.Ready) -> None:
        raise NotImplementedError

    def _task_event(self, event: pb.ServerEvent) -> EventT | None:
        """The caller's event for a task body, or ``None`` for a body this session does not know."""
        raise NotImplementedError

    # --- the caller's view ----------------------------------------------------

    @property
    def session_id(self) -> str:
        return self._session_id

    @property
    def unknown_events(self) -> int:
        """Server events passed over because this client does not know their kind."""
        return self._unknown_events

    @property
    def limits(self) -> SessionLimits:
        limits = self._opened().limits
        stt = None
        if limits.HasField("speech_to_text"):
            bounds = limits.speech_to_text
            stt = SpeechToTextLimits(
                bounds.min_frame_ms, bounds.max_frame_ms, bounds.max_utterance_ms
            )
        return SessionLimits(
            idle_timeout_ms=limits.idle_timeout_ms,
            heartbeat_interval_ms=limits.heartbeat_interval_ms,
            liveness_timeout_ms=limits.liveness_timeout_ms,
            max_duration_ms=limits.max_duration_ms,
            finalize_deadline_ms=limits.finalize_deadline_ms,
            output_stall_timeout_ms=limits.output_stall_timeout_ms,
            speech_to_text=stt,
        )

    @property
    def remaining_duration_ms(self) -> int:
        """Time the session had left when it became ready."""
        return int(self._opened().remaining_duration_ms)

    async def open(self) -> None:
        """Send the open and wait for the server to admit the session.

        Raises the typed error of a refusal, and ``SessionTimeoutError`` when
        no answer arrives in time. Entering the session with ``async with``
        calls this.
        """
        if self._call is not None:
            raise SessionClosedError("the session was already opened")
        target = self._target
        opening = self._open_body(
            pb.ResolvedTarget(
                deployment_id=target.deployment_id,
                generation=target.generation,
                descriptor_digest=target.descriptor_digest,
            )
        )
        self._call = self._stub.Session()
        self._reader = asyncio.create_task(self._read())
        try:
            await self._send(pb.ClientEvent(open=opening))
            await self._until(lambda: self._ready is not None, self._open_timeout, "answer to Open")
        except BaseException:
            self._call.cancel()
            raise

    async def close(self, *, timeout: float | None = None) -> SessionTotals:
        """Close the sending half, let the server finish what it accepted, and return the totals.

        Events that arrive meanwhile are still held for iteration, so a
        session whose held events are at their bound ends only once some are
        taken, or `timeout` seconds pass. Raises the session's error if it
        failed. Leaving ``async with`` without an exception calls this.
        """
        self._opened()
        async with self._write_lock:
            if not self._ended and not self._half_closed:
                self._half_closed = True
                self._stop_ping()
                try:
                    await self._call.done_writing()
                except BaseException as error:
                    if not self._call_ended(error):
                        raise
        await self._until(lambda: self._ended, timeout, "end of the session")
        if self._failure is not None:
            raise self._failure
        if self._closed is None or self._cancelling:
            raise self._end_error()
        totals = self._closed.totals
        return SessionTotals(
            totals.input_audio_samples,
            totals.input_text_bytes,
            totals.output_audio_samples,
            totals.utterances_completed,
            totals.segments_completed,
        )

    async def cancel(self, *, timeout: float = 5.0) -> None:
        """Abandon the session.

        Drops every event not yet taken and every one that still arrives,
        tells the server, and waits up to `timeout` seconds for it to end the
        session before ending the call. Leaving ``async with`` on an exception
        calls this.
        """
        if self._call is None or self._ended:
            return
        self._cancelling = True
        self._events.end(discard=True)
        self._changed.notify()

        async def ask() -> None:
            await self._send(pb.ClientEvent(cancel=pb.Cancel()))
            await self._until_ended()

        try:
            if not self._half_closed:
                # One bound for both: a writer held by the transport holds the Cancel back too.
                with contextlib.suppress(SpeechError, asyncio.TimeoutError):
                    await asyncio.wait_for(ask(), timeout)
        finally:
            self._call.cancel()
        await self._until_ended()

    async def __aenter__(self: SessionT) -> SessionT:
        await self.open()
        return self

    async def __aexit__(
        self,
        kind: type[BaseException] | None,
        error: BaseException | None,
        traceback: TracebackType | None,
    ) -> None:
        if kind is None:
            await self.close()
        else:
            await self.cancel()

    def __aiter__(self: SessionT) -> SessionT:
        return self

    async def __anext__(self) -> EventT:
        event = await self._events.get()
        if event is not None:
            return event
        await self._until_ended()
        if self._failure is not None:
            raise self._failure
        raise StopAsyncIteration

    # --- sending --------------------------------------------------------------

    def _opened(self) -> pb.Ready:
        if self._ready is None:
            raise SessionClosedError("the session is not open")
        return self._ready

    def _check_sending(self) -> None:
        self._opened()
        if self._ended or self._cancelling:
            raise self._end_error()
        if self._half_closed:
            raise SessionClosedError("the sending half is closed", admitted=True)

    def _end_error(self) -> SpeechError:
        return self._failure or SessionClosedError("the session has ended", admitted=True)

    def _violation(self, what: str) -> StreamProtocolError:
        admitted = True if self._ready is not None or self._closed is not None else None
        return StreamProtocolError(
            f"the server broke the stream protocol: {what}", admitted=admitted
        )

    async def _send(
        self, event: pb.ClientEvent, *, charge: int | None = None, answered: bool = False
    ) -> int:
        """Write one client event and return its sequence.

        `charge` is the input credit an Audio or a TextSegment takes;
        `answered` marks a Finalize, whose answer names the sequence.
        """
        # One writer at a time: gRPC ends a call given two writes at once, and
        # the sequence must be the order on the wire.
        async with self._write_lock:
            if self._ended or self._half_closed:
                raise self._end_error()
            self._sent += 1
            sequence = self._sent
            if charge is not None:
                self._unaccepted[sequence] = charge
                self._last_input = sequence
            if answered:
                self._finalizes.add(sequence)
            event.sequence, event.session_id = sequence, self._session_id
            try:
                await self._call.write(event)
            except BaseException as error:
                if not self._call_ended(error):
                    raise
                # The reader has the reason unless it is held behind a full
                # buffer, so it gets a moment and no more.
                if self._reader is not None:
                    await asyncio.wait({self._reader}, timeout=1.0)
                await self._conclude()
                raise self._end_error() from None
            return sequence

    def _call_ended(self, error: BaseException) -> bool:
        """Whether `error`, raised by a write to the call, means the call has ended."""
        if isinstance(error, asyncio.CancelledError):
            # gRPC raises it for a call this side cancelled; otherwise it is the task's own.
            return self._cancelling or self._abort_error is not None
        return isinstance(error, (grpc.aio.AioRpcError, asyncio.InvalidStateError))

    def _fits(self, size: int) -> bool:
        """Whether the credit last reported, less what was sent since, admits `size` more bytes."""
        return self._credit_used + sum(self._unaccepted.values()) + size <= self._credit_limit

    async def _finalize(self, timeout: float | None) -> EventT:
        """Send a Finalize and return the event that answers it; the caller holds the input gate."""
        self._check_sending()
        sequence = await self._send(pb.ClientEvent(finalize=pb.Finalize()), answered=True)
        await self._until(lambda: sequence in self._answers, timeout, "answer to Finalize")
        return self._answers.pop(sequence)

    async def _until(self, done: Callable[[], bool], timeout: float | None, what: str) -> None:
        """Wait for `done`; a session that ends first raises its error, and a timeout ends it."""

        async def wait() -> None:
            while not done() and not self._ended:
                await self._changed.wait()

        try:
            await asyncio.wait_for(wait(), timeout)
        except asyncio.TimeoutError:
            admitted = True if self._ready is not None else None
            self._abort(SessionTimeoutError(f"no {what} within {timeout} s", admitted=admitted))
            await self._until_ended()
        if not done():
            raise self._end_error()

    async def _until_ended(self) -> None:
        while not self._ended:
            await self._changed.wait()

    def _abort(self, error: SpeechError) -> None:
        """End the call from this side; the session's error is `error` unless one came first."""
        if self._abort_error is None:
            self._abort_error = error
        self._events.end()
        self._call.cancel()

    def _stop_ping(self) -> None:
        if self._pinger is not None:
            self._pinger.cancel()

    async def _ping(self, interval: float) -> None:
        with contextlib.suppress(SpeechError):
            while True:
                await asyncio.sleep(interval)
                await self._send(pb.ClientEvent(ping=pb.Ping()))

    # --- receiving ------------------------------------------------------------

    async def _read(self) -> None:
        try:
            while True:
                event = await self._call.read()
                if event is grpc.aio.EOF:
                    break
                await self._take(event)
        except grpc.aio.AioRpcError:
            pass
        except StreamProtocolError as violation:
            self._abort(violation)
        except asyncio.CancelledError:
            # Raised by a read once this side cancelled the call, which is not this task's own.
            if not self._call.cancelled():
                raise
        await self._conclude()

    async def _take(self, event: pb.ServerEvent) -> None:
        self._received += 1
        if event.sequence != self._received:
            raise self._violation(f"event {self._received} carries sequence {event.sequence}")
        if self._closed is not None:
            raise self._violation("an event after SessionClosed")
        body = event.WhichOneof("body")
        if body is None:
            self._unknown_events += 1
            return
        if body != "session_closed" and (self._ready is None) != (body == "ready"):
            raise self._violation(f"{body} {'before' if self._ready is None else 'after'} Ready")
        message = getattr(event, body)
        if body == "ready":
            self._take_ready(message)
        elif body == "accepted":
            self._take_accepted(message)
        elif body == "session_closed":
            if message.state not in (pb.SESSION_STATE_CLOSED, pb.SESSION_STATE_FAILED):
                raise self._violation("SessionClosed in a state that is not an end")
            if message.admission != pb.ADMISSION_ADMITTED:
                raise self._violation("SessionClosed for a call that was not admitted")
            self._closed = message
        elif body == "cancel_accepted":
            if not self._cancelling:
                raise self._violation("CancelAccepted without a Cancel")
        elif body not in ("pong", "status"):
            item = self._task_event(event)
            if item is None:
                self._unknown_events += 1
            else:
                self._changed.notify()
                await self._events.put(item, event.ByteSize())
        self._changed.notify()

    def _take_ready(self, ready: pb.Ready) -> None:
        target = self._target
        if not ready.session_id:
            raise self._violation("Ready names no session")
        if (ready.generation, ready.descriptor_digest) != (
            target.generation,
            target.descriptor_digest,
        ):
            raise self._violation("Ready names another generation or descriptor")
        if not ready.limits.heartbeat_interval_ms:
            raise self._violation("Ready gives no heartbeat interval")
        self._take_credit(ready.status.input_credit)
        self._check_ready(ready)
        self._session_id, self._ready = ready.session_id, ready
        interval = ready.limits.heartbeat_interval_ms / 1000
        self._pinger = asyncio.create_task(self._ping(interval))

    def _take_accepted(self, accepted: pb.Accepted) -> None:
        if not self._accepted <= accepted.accepted_sequence <= self._last_input:
            raise self._violation("Accepted names a sequence no input carries")
        self._accepted = accepted.accepted_sequence
        for sequence in [s for s in self._unaccepted if s <= self._accepted]:
            del self._unaccepted[sequence]
        self._take_credit(accepted.input_credit)

    def _take_credit(self, credit: pb.InputCredit) -> None:
        if not credit.bytes.limit:
            raise self._violation("no byte budget in the input credit")
        self._credit_used, self._credit_limit = credit.bytes.used, credit.bytes.limit

    async def _conclude(self) -> None:
        """Fix the session's outcome from the ended call; the first conclusion stands."""
        status = await self._call.code()
        refusal = None
        if not self._received and status is not grpc.StatusCode.OK:
            refusal = decode_refusal(await self._call.trailing_metadata())
        if self._ended:
            return
        closed = self._closed
        cause = closed.cause if closed is not None and closed.HasField("cause") else None
        admitted = True if self._ready is not None or closed is not None else None
        if refusal is not None:
            cause = refusal.cause
            admitted = False if refusal.admission == pb.ADMISSION_NOT_ADMITTED else None
        if self._cancelling:
            self._failure = None
        elif self._abort_error is not None:
            self._failure = self._abort_error
        elif status is not grpc.StatusCode.OK:
            self._failure = failure_of(status, cause, admitted)
        elif closed is None:
            self._failure = self._violation("the call ended with status OK and no SessionClosed")
        elif cause is not None:
            self._failure = self._violation("the call ended with status OK and a cause")
        self._ended = True
        self._stop_ping()
        self._events.end()
        self._changed.notify()

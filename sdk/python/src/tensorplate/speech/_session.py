"""One call of the stream schema's ``Session``: the exchange every kind of session shares."""

from __future__ import annotations

import asyncio
import contextlib
from collections.abc import Callable, Coroutine
from types import TracebackType
from typing import Any, Generic, Protocol, TypeVar

import grpc
import grpc.aio

from tensorplate.errors import ErrorCode
from tensorplate.speech._buffer import EventBuffer, Signal
from tensorplate.speech.discovery import StreamTarget
from tensorplate.speech.errors import (
    DiscoveryError,
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
from tensorplate.speech.events import (
    SessionLimits,
    SessionTotals,
    SpeechToTextLimits,
    Target,
    TextToSpeechLimits,
)
from tensorplate.stream.v1 import session_pb2 as pb

EventT = TypeVar("EventT")
ResultT = TypeVar("ResultT")
SessionT = TypeVar("SessionT", bound="Session[Any]")


class Targets(Protocol):
    """Where a client made from the agent's status opens its sessions."""

    async def fresh(self) -> StreamTarget: ...

    async def after(self, seen: StreamTarget) -> StreamTarget: ...

    def stale(self) -> None: ...

    def connector(self, endpoint: str) -> Callable[[], Any]: ...


# Transcripts and control events a session holds for its caller before it stops reading.
METADATA_BUFFER_BYTES = 16 * 1024

# A session ended with one of these reasons was served by a worker that is going away.
_MOVED = ("worker_shutdown", "deployment_retired")

# What a write raises once its call has ended. The CancelledError among them is gRPC's, for a
# call this side cancelled, or a write given up before Ready (`Session._write`): a session's
# writing task is never cancelled (`Session._within`).
_CALL_ENDED = (grpc.RpcError, grpc.aio.BaseError, asyncio.InvalidStateError, asyncio.CancelledError)

_ERROR_OF_STATUS: dict[str, type[SpeechError]] = {
    "INVALID_ARGUMENT": InvalidEventError,
    "FAILED_PRECONDITION": PreconditionFailedError,
    "UNIMPLEMENTED": PreconditionFailedError,
    "RESOURCE_EXHAUSTED": ResourceExhaustedError,
    "DEADLINE_EXCEEDED": SessionTimeoutError,
    "UNAVAILABLE": ServiceUnavailableError,
    "CANCELLED": SessionCancelledError,
}

# The status an error code stands for: the stream listener's rule for a cause whose reason
# its own mapping does not name (docs/architecture/serving-worker.md, "Final status").
_STATUS_OF_CODE: dict[ErrorCode, str] = {
    ErrorCode.CONFIG_INVALID: "INVALID_ARGUMENT",
    ErrorCode.SHAPE_MISMATCH: "INVALID_ARGUMENT",
    ErrorCode.UNSUPPORTED: "FAILED_PRECONDITION",
    ErrorCode.OOM_ERROR: "RESOURCE_EXHAUSTED",
    ErrorCode.RESOURCE_EXHAUSTED: "RESOURCE_EXHAUSTED",
    ErrorCode.TIMEOUT: "DEADLINE_EXCEEDED",
    ErrorCode.LOAD_FAILED: "UNAVAILABLE",
    ErrorCode.NOT_READY: "UNAVAILABLE",
    ErrorCode.UNAVAILABLE: "UNAVAILABLE",
    ErrorCode.CANCELLED: "CANCELLED",
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
    status: grpc.StatusCode,
    cause: pb.EndCause | None,
    admitted: bool | None,
    *,
    write_failed: bool = False,
) -> SpeechError:
    """The error for a call that ended with `status`; its class follows the status code.

    One case takes the class from the cause. A write that reaches gRPC after
    the server ended the call, and before the client read that end, makes
    gRPC report ``INTERNAL`` in place of the server's status. So with
    `write_failed`, ``INTERNAL`` and a cause, the class is the one the
    cause's error code stands for; ``grpc_status`` stays what gRPC reported.
    """
    reason = code = None
    if cause is not None:
        reason = enum_name(pb.FailureReason, cause.reason)
        with contextlib.suppress(ValueError):
            code = ErrorCode(enum_name(pb.ErrorCode, cause.code))
    name = status.name
    if write_failed and status is grpc.StatusCode.INTERNAL and code is not None:
        name = _STATUS_OF_CODE.get(code, name)
    kind = _ERROR_OF_STATUS.get(name, ServerInternalError)
    return kind(
        f"the session ended with status {status.name}" + (f" ({reason})" if reason else ""),
        reason=reason,
        code=code,
        grpc_status=status.name,
        admitted=admitted,
    )


def _retrieve(task: asyncio.Future[Any]) -> None:
    """Take the outcome of a task nobody awaits any more, so that asyncio does not report it."""
    if not task.cancelled():
        task.exception()


class Session(Generic[EventT]):
    """What both kinds of session share. Obtained from a ``SpeechClient``, never constructed.

    Iterating a session yields its events in the order the server sent them
    and ends when the session does; a session that failed raises its error
    once the events read before the failure are taken.

    Events must be read while the session sends. Credit, the answer to a
    Finalize and the end of the session all arrive on the stream the events
    arrive on, and the session stops reading it once it holds 16 KiB of
    events its caller has not taken, or two seconds of audio in a
    text-to-speech session. A session left that way for the
    server's output stall time (``limits.output_stall_timeout_ms``) is ended
    with ``ResourceExhaustedError``. That timer runs only while the session
    has stopped reading for such events and starts again with each one
    taken, so it can end a session before the server's own timer would.

    A task cancelled inside a method gets its ``CancelledError`` and the
    session stays open; an event it was writing at that moment is still sent.
    A task cancelled on the turn of the event loop in which its call fails
    gets ``CancelledError`` too, and that call's error is not raised. An
    error that ended the session is raised by the session's next use; after
    an ``open()`` that failed, by iterating the session, while its other
    calls raise ``SessionClosedError``. A ``ValueError`` for input that was
    not sent is not reported at all.
    """

    # Whether the session's input is text segments, which the waiting-segment credit bounds.
    _sends_segments = False

    def __init__(self) -> None:
        raise TypeError("a session is obtained from a SpeechClient, not constructed")

    def _setup(
        self,
        connect: Callable[[], Any],
        target: Target,
        open_timeout: float | None,
        close_timeout: float | None,
        cancel_timeout: float,
        targets: Targets | None = None,
    ) -> None:
        self._connect, self._target, self._targets = connect, target, targets
        self._resolved: StreamTarget | None = None
        self._open_timeout, self._close_timeout = open_timeout, close_timeout
        self._cancel_timeout = cancel_timeout
        self._call: Any = None
        self._reader: asyncio.Task[None] | None = None
        self._pinger: asyncio.Task[None] | None = None
        self._write_lock = asyncio.Lock()
        self._writer: asyncio.Task[Any] | None = None
        # Held by one input or one finalize at a time, so that input goes out in one order.
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
        self._write_failed = False
        self._credit_used = self._credit_limit = 0
        self._segments_waiting = self._segment_limit = 0
        self._unaccepted: dict[int, int] = {}
        self._last_input = self._accepted = 0
        # The Finalize whose answer has not arrived, whoever waits for it, and the last answer.
        self._finalizing: int | None = None
        self._answer: tuple[int, EventT] | None = None
        self._unknown_events = 0
        # While `open()` runs, once it made its second call, and the failure of its first
        # call while it makes the second, with whether that failure alone allows the second.
        self._opening = self._reopened = False
        self._lost: SpeechError | None = None
        self._lost_retryable = False

    # --- what a subclass supplies ---------------------------------------------

    def _open_body(self, resolved: pb.ResolvedTarget) -> pb.Open:
        raise NotImplementedError

    def _check_ready(self, ready: pb.Ready) -> None:
        raise NotImplementedError

    def _task_event(self, event: pb.ServerEvent) -> EventT | None:
        """The caller's event for a task body, or ``None`` for a body this session does not know."""
        raise NotImplementedError

    def _sending(self, event: pb.ClientEvent) -> None:
        """Called with the write lock held, just before `event` is written."""

    def _check_closed(self, closed: pb.SessionClosed) -> None:
        """Hold the server to what the session read before its ``SessionClosed``."""

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
        tts = None
        if limits.HasField("text_to_speech"):
            text = limits.text_to_speech
            tts = TextToSpeechLimits(
                text.max_segment_text_bytes,
                text.max_segment_audio_ms,
                text.max_synthesis_text_bytes,
                text.max_synthesis_audio_ms,
            )
        return SessionLimits(
            idle_timeout_ms=limits.idle_timeout_ms,
            heartbeat_interval_ms=limits.heartbeat_interval_ms,
            liveness_timeout_ms=limits.liveness_timeout_ms,
            max_duration_ms=limits.max_duration_ms,
            finalize_deadline_ms=limits.finalize_deadline_ms,
            output_stall_timeout_ms=limits.output_stall_timeout_ms,
            speech_to_text=stt,
            text_to_speech=tts,
        )

    @property
    def remaining_duration_ms(self) -> int:
        """Time the session had left when it became ready."""
        return int(self._opened().remaining_duration_ms)

    async def open(self) -> None:
        """Send the open and wait for the server to admit the session.

        Raises the typed error of a refusal, ``SessionTimeoutError`` when no
        answer arrives within the session's `open_timeout`, and
        ``SessionClosedError`` for a session that was opened before or whose
        client or channel is closed. Entering the session with ``async
        with`` calls this. A caller cancelled on the turn this call fails
        gets ``CancelledError``, and the call's error is not raised.

        A call that ends before any server event, with ``UNAVAILABLE`` or
        with ``INTERNAL`` after its Open failed to be written (gRPC's status
        for a write to a call that has ended), is made once more within the
        same `open_timeout`. A connection lost as the Open is sent ends a
        call that way, and the call carried nothing but the Open. The
        session is not ended in between, so a cancel or a close then ends
        it with no second call.

        A session of a client made by ``SpeechClient.from_agent()`` may read
        the agent's status first, and between the two calls, and then raises
        a ``DiscoveryError`` when the status names no stream target; its
        second call may go to the target the agent names then. It makes no
        more than two calls either way.
        """
        if self._opening or self._call is not None or self._ended:
            raise SessionClosedError("the session was opened before, or its client is closed")
        self._opening = True
        try:
            await self._bounded(self._begin(), self._open_timeout, "answer to Open")
        except BaseException:
            if self._call is not None:
                self._call.cancel()
            elif not self._ended:
                self._abort(
                    SessionClosedError("the open ended before its first call", admitted=False)
                )
            raise
        finally:
            self._opening = False
            if self._lost is not None:
                self._settle(self._lost)

    async def close(self, *, timeout: float | None = None) -> SessionTotals:
        """Close the sending half, let the server finish what it accepted, and return the totals.

        The end of the session is read from the server after the events
        that precede it, so they must be read meanwhile; they are still held
        for iteration. `timeout`, in seconds, bounds the whole call, and the
        session's `close_timeout` does when none is given: past it the call
        is cancelled and ``SessionTimeoutError`` raised. Raises the
        session's error if it failed. Leaving ``async with`` without an
        exception calls this. A caller cancelled on the turn this call
        fails gets ``CancelledError``, and the call's error is not raised.
        """
        self._opened()
        bound = self._close_timeout if timeout is None else timeout
        await self._bounded(self._drain(), bound, "end of the session")
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

    async def cancel(self, *, timeout: float | None = None) -> None:
        """Abandon the session.

        Drops every event not yet taken and every one that still arrives,
        tells the server, and waits up to `timeout` seconds (the session's
        `cancel_timeout` when none is given) for it to end the session
        before ending the call. Leaving ``async with`` on an exception, or
        cancelled, calls this.
        """
        if self._ended:
            return
        if self._call is None:
            # An open still reading its target from the agent sends no Open once it has read it.
            if self._opening:
                self._cancelling = True
                self._settle(None)
            return
        self._cancelling = True
        if self._lost is not None:
            self._settle(None)
        self._events.end(discard=True)
        self._changed.notify()
        bound = self._cancel_timeout if timeout is None else timeout
        try:
            if not self._half_closed:
                # One bound for both: a writer held by the transport holds the Cancel back too.
                with contextlib.suppress(SpeechError, asyncio.TimeoutError):
                    await self._within(self._ask_to_end(), bound)
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
        try:
            if kind is None:
                await self.close()
        finally:
            # Does nothing once the session has ended; an exception or a cancelled close
            # leaves it open.
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

    async def _within(self, work: Coroutine[Any, Any, ResultT], timeout: float | None) -> ResultT:
        """Run `work` as a task; ``asyncio.TimeoutError`` once `timeout` seconds from now pass.

        ``asyncio.wait`` raises ``CancelledError`` for the calling task's own
        cancellation and for nothing else, so the caller is never told that
        it was cancelled when gRPC cancelled the call, nor the reverse. Work
        left unfinished is cancelled, unless it is writing: gRPC cancels a
        call when a write on it is cancelled, so that write runs to its end,
        or before Ready to the end of its call (`_write`).
        """
        task = asyncio.ensure_future(work)
        try:
            done, _ = await asyncio.wait({task}, timeout=timeout)
        finally:
            if not task.done() and task is not self._writer:
                task.cancel()
            # A caller cancelled here leaves the work's outcome, which may already be there.
            task.add_done_callback(_retrieve)
        if not done:
            raise asyncio.TimeoutError
        return task.result()

    async def _bounded(
        self, work: Coroutine[Any, Any, ResultT], timeout: float | None, what: str
    ) -> ResultT:
        """Run `work` under one deadline, `timeout` seconds from now on the loop's clock.

        The deadline covers all of it: the wait for its turn, its write and
        its answer. Past it the session is ended with ``SessionTimeoutError``.
        """
        try:
            return await self._within(work, timeout)
        except asyncio.TimeoutError:
            admitted = True if self._ready is not None else None
            self._abort(SessionTimeoutError(f"no {what} within {timeout} s", admitted=admitted))
            await self._until_ended()
            raise self._end_error() from None

    def _connected(self) -> grpc.aio.StreamStreamCall:
        """A new call for the session, with a reader started for it."""
        try:
            call = self._connect()
        except grpc.aio.UsageError:
            self._shut()
            raise SessionClosedError("the client's channel is closed") from None
        self._reader = asyncio.create_task(self._read())
        return call

    async def _begin(self) -> None:
        if self._targets is not None:
            try:
                found = await self._targets.fresh()
            except DiscoveryError as error:
                self._settle(error)
                raise self._end_error() from None
            if not self._ended:
                self._use(found)
        # A cancel or a close before the first call leaves none to make.
        if self._ended:
            raise self._end_error()
        self._call = self._connected()
        try:
            await self._send(pb.ClientEvent(open=self._open_event()))
            await self._until(lambda: self._ready is not None)
            return
        except SpeechError:
            if self._lost is None:
                raise
        # Outside the handler, so that the second call's error has no context.
        if self._targets is not None:
            await self._follow(self._lost)
        # `_lost` is cleared once the new call exists, so a closed channel ends the session.
        self._call = self._connected()
        self._lost, self._reopened = None, True
        self._write_failed, self._sent = False, 0
        await self._send(pb.ClientEvent(open=self._open_event()))
        await self._until(lambda: self._ready is not None)

    async def _follow(self, lost: SpeechError) -> None:
        """Read the target again after the first call was `lost`; raise if no second call follows.

        The second call goes to the target the agent names now when it
        changed, and to the same one only when the loss alone allows it.
        """
        targets, seen = self._targets, self._resolved
        assert targets is not None
        assert seen is not None
        unread: DiscoveryError | None = None
        try:
            found = await targets.after(seen)
        except DiscoveryError as error:
            unread = error
        if unread is not None:
            # The next open reads before its first call, which the old target just lost.
            targets.stale()
            unread.admitted, unread.__cause__ = lost.admitted, lost
            self._settle(unread)
        if self._ended:
            raise self._end_error()
        if found == seen and not self._lost_retryable:
            self._settle(lost)
            raise self._end_error()
        self._use(found)

    def _use(self, resolved: StreamTarget) -> None:
        assert self._targets is not None
        self._resolved, self._target = resolved, resolved.target
        self._connect = self._targets.connector(resolved.endpoint)

    def _open_event(self) -> pb.Open:
        target = self._target
        return self._open_body(
            pb.ResolvedTarget(
                deployment_id=target.deployment_id,
                generation=target.generation,
                descriptor_digest=target.descriptor_digest,
            )
        )

    async def _drain(self) -> None:
        async with self._write_lock:
            if not self._ended and not self._half_closed:
                self._half_closed = True
                self._stop_ping()
                self._writer = asyncio.current_task()
                try:
                    with contextlib.suppress(*_CALL_ENDED):
                        await self._call.done_writing()
                finally:
                    self._writer = None
        await self._until_ended()

    async def _ask_to_end(self) -> None:
        await self._send(pb.ClientEvent(cancel=pb.Cancel()))
        await self._until_ended()

    async def _send(
        self, event: pb.ClientEvent, *, charge: int | None = None, finalize: bool = False
    ) -> int:
        """Write one client event and return its sequence.

        `charge` is the input credit an Audio or a TextSegment takes;
        `finalize` marks a Finalize, which holds input back until its answer.
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
            if finalize:
                self._finalizing = sequence
            event.sequence, event.session_id = sequence, self._session_id
            self._sending(event)
            self._writer = asyncio.current_task()
            try:
                await self._write(event)
            except _CALL_ENDED:
                # The reader has the reason, and is let past a full buffer to reach it. The
                # cancel is for a call that a failed write left open, if gRPC ever does.
                self._write_failed = True
                # Before Ready nothing is held, and an open may make a second call on this buffer.
                if self._ready is not None:
                    self._events.unbound()
                self._call.cancel()
                await self._until_ended()
                raise self._end_error() from None
            finally:
                self._writer = None
            return sequence

    async def _write(self, event: pb.ClientEvent) -> None:
        """Write `event`; before Ready, give the write up once the call has ended.

        gRPC never ends a write on a call cancelled before its first batch
        was sent, as an open's timeout or its caller's cancellation can do.
        A call ended before Ready is abandoned, so the write is not needed.
        """
        if self._ready is not None:
            await self._call.write(event)
            return

        async def attempt() -> None:
            try:
                await self._call.write(event)
            except _CALL_ENDED:
                # In the step gRPC fails the write in, before the reader can read that
                # INTERNAL: `_conclude` must see the write as failed.
                self._write_failed = True
                raise

        write = asyncio.ensure_future(attempt())
        write.add_done_callback(_retrieve)
        ended = asyncio.ensure_future(self._until_ended())
        try:
            await asyncio.wait({write, ended}, return_when=asyncio.FIRST_COMPLETED)
        finally:
            ended.cancel()
        if not write.done():
            write.cancel()
        # One given up raises CancelledError, which `_send` takes as the end of the call.
        await write

    def _fits(self, size: int) -> bool:
        """Whether the credit last reported, less what was sent since, admits `size` more bytes.

        In a text-to-speech session those bytes are one more waiting segment.
        """
        sent = self._unaccepted
        if self._credit_used + sum(sent.values()) + size > self._credit_limit:
            return False
        return not self._sends_segments or self._segments_waiting + len(sent) < self._segment_limit

    async def _send_finalize(self) -> int:
        """Send a Finalize once none is unanswered; the caller holds the input gate."""
        # A Finalize repeated before the answer has none of its own.
        await self._until(lambda: self._finalizing is None)
        self._check_sending()
        return await self._send(pb.ClientEvent(finalize=pb.Finalize()), finalize=True)

    async def _answer_to(self, sequence: int) -> EventT:
        await self._until(lambda: self._answer is not None and self._answer[0] == sequence)
        assert self._answer is not None
        return self._answer[1]

    def _take_answer(self, sequence: int, answer: EventT) -> None:
        """Record the event that answers the Finalize `sequence`; input may follow it."""
        if sequence != self._finalizing:
            raise self._violation("an answer to a Finalize that is not waiting")
        self._finalizing, self._answer = None, (sequence, answer)

    async def _until(self, done: Callable[[], bool]) -> None:
        """Wait for `done`; a session that ends first, or loses its open's call, raises."""
        while not done() and not self._ended and self._lost is None:
            await self._changed.wait()
        if not done():
            raise self._end_error()

    async def _until_ended(self) -> None:
        """Wait for the session's end, or for its open to lose its first call."""
        while not self._ended and self._lost is None:
            await self._changed.wait()

    def _abort(self, error: SpeechError) -> None:
        """End the call from this side; the session's error is `error` unless one came first."""
        if self._abort_error is None:
            self._abort_error = error
        if self._call is None or self._lost is not None:
            self._settle(None)
        self._events.end()
        if self._call is not None:
            self._call.cancel()

    def _shut(self) -> None:
        """End the session because its client or channel closed; unread events are dropped."""
        self._cancelling = True
        self._events.end(discard=True)
        if self._call is None:
            self._ended = True
        elif self._lost is not None:
            self._settle(None)
        else:
            self._call.cancel()
        self._changed.notify()

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
                if not isinstance(event, pb.ServerEvent):
                    # gRPC hands back nothing for bytes that do not decode, and reads on.
                    raise self._violation("a message that is not a server event")
                await self._take(event)
        except (grpc.RpcError, asyncio.CancelledError):
            # The call ended with a status, or was cancelled: by this side, or with this task.
            pass
        except SpeechError as error:
            self._abort(error)
        except Exception as error:
            violation = self._violation(f"reading the call failed with {type(error).__name__}")
            violation.__cause__ = error
            self._abort(violation)
        finally:
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
            self._check_closed(message)
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
                audio = body == "audio_chunk"
                await self._hold(item, len(message.data) if audio else event.ByteSize(), audio)
        self._changed.notify()

    async def _hold(self, item: EventT, size: int, audio: bool) -> None:
        """Keep `item` for the caller, waiting for room no longer than the server would."""
        patience = self._opened().limits.output_stall_timeout_ms / 1000
        try:
            await self._events.put(item, size, patience, audio=audio)
        except asyncio.TimeoutError:
            raise ResourceExhaustedError(
                f"the session's events were left unread for {patience} s; "
                "read them while the session sends",
                admitted=True,
            ) from None

    def _take_ready(self, ready: pb.Ready) -> None:
        target = self._target
        if not ready.session_id:
            raise self._violation("Ready names no session")
        if (ready.generation, ready.descriptor_digest) != (
            target.generation,
            target.descriptor_digest,
        ):
            raise self._violation("Ready names another generation or descriptor")
        self._take_credit(ready.status.input_credit)
        limits = ready.limits
        # Without it nothing would end a session whose events nobody reads.
        if not limits.output_stall_timeout_ms:
            raise self._violation("Ready declares no output stall time")
        # Without it no Ping would be sent, and the server would end the session for that.
        if not limits.heartbeat_interval_ms:
            raise self._violation("Ready declares no heartbeat interval")
        self._check_ready(ready)
        self._session_id, self._ready = ready.session_id, ready
        self._pinger = asyncio.create_task(self._ping(limits.heartbeat_interval_ms / 1000))

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
        waiting = credit.waiting_segments
        if self._sends_segments and not waiting.limit:
            raise self._violation("no waiting-segment budget in the input credit")
        self._credit_used, self._credit_limit = credit.bytes.used, credit.bytes.limit
        self._segments_waiting, self._segment_limit = waiting.used, waiting.limit

    async def _conclude(self) -> None:
        """Fix the session's outcome from the ended call and wake everything that waits on it."""
        # Ends a call the reader left open, which it does only when cancelled itself.
        self._call.cancel()
        status = await self._call.code()
        refusal = None
        if not self._received and status is not grpc.StatusCode.OK:
            refusal = decode_refusal(await self._call.trailing_metadata())
        closed = self._closed
        cause = closed.cause if closed is not None and closed.HasField("cause") else None
        admitted = True if self._ready is not None or closed is not None else None
        if refusal is not None:
            cause = refusal.cause
            admitted = False if refusal.admission == pb.ADMISSION_NOT_ADMITTED else None
        failure = None
        if status is not grpc.StatusCode.OK:
            failure = failure_of(status, cause, admitted, write_failed=self._write_failed)
        elif closed is None:
            failure = self._violation("the call ended with status OK and no SessionClosed")
        elif cause is not None:
            failure = self._violation("the call ended with status OK and a cause")
        lost = failure is not None and (
            failure.grpc_status == "UNAVAILABLE"
            or (failure.grpc_status == "INTERNAL" and self._write_failed)
        )
        # A call ended so before any server event was lost with its connection, and carried
        # only the Open: the open makes one more call, and the session is not ended meanwhile.
        # An open on the agent's target holds any such end, to read the target again.
        reopen = self._opening and not self._reopened and not self._received
        follows = lost or (self._targets is not None and status is not grpc.StatusCode.OK)
        if follows and reopen and not self._cancelling and self._abort_error is None:
            self._lost, self._lost_retryable = failure, lost
            self._changed.notify()
            return
        self._settle(failure)

    def _settle(self, failure: SpeechError | None) -> None:
        """End the session with `failure`, unless it was cancelled or aborted first."""
        self._lost = None
        if self._targets is not None and failure is not None and failure.reason in _MOVED:
            self._targets.stale()
        if self._cancelling:
            failure = None
        elif self._abort_error is not None:
            failure = self._abort_error
        self._failure = failure
        self._ended = True
        self._stop_ping()
        self._events.end()
        self._changed.notify()

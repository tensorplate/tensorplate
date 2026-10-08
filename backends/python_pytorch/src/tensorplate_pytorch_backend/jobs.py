"""The job table of one connection and the lane its backend work waits in.

The reader thread admits, cancels and releases; the backend thread takes work
from the lane, one item at a time, and ends each job it ran. Every job message
is written while the table lock is held, so a job's wire order is the order of
its state changes. Lock order: the table lock, then the runner's write lock.
"""

from __future__ import annotations

import logging
import threading
from collections import deque
from collections.abc import Callable
from dataclasses import dataclass
from typing import Any, Final

from tensorplate_pytorch_backend import codec, job_objects, protocol, sanitize
from tensorplate_pytorch_backend.backends.base import BackendError, JobBackend

logger = logging.getLogger("tensorplate.sidecar")

#: Jobs that may wait behind the running one.
GPU_LANE_QUEUE_DEPTH: Final[int] = 8
#: Unary requests that may wait for the backend thread.
UNARY_QUEUE_DEPTH: Final[int] = 8
#: The job classes that have a lane, in job-class order.
LANE_JOB_CLASSES: Final[tuple[str, ...]] = (
    protocol.JOB_CLASS_STT_DECODE,
    protocol.JOB_CLASS_TTS_SYNTHESIS,
)


@dataclass(eq=False, slots=True)
class Job:
    identity: job_objects.JobIdentity
    sequence: job_objects.JobEventSequence
    #: What an admitted job runs with, until it is released.
    request: job_objects.JobRequest | None = None
    backend: JobBackend | None = None
    cancelled: bool = False

    @property
    def session(self) -> tuple[int, int]:
        return self.identity.session_key, self.identity.generation


def _message(kind: str, message_id: str = "", **fields: object) -> codec.SidecarFrame:
    envelope = {"schema_version": protocol.SCHEMA_VERSION, "message_id": message_id, "kind": kind}
    return codec.SidecarFrame({**envelope, **fields})


def _edge(exc: BaseException) -> sanitize.EdgeError:
    if not isinstance(exc, BackendError):
        logger.error("sidecar job failed: %s", sanitize.describe(exc))
    return sanitize.edge_error(exc)


def _permits(backend: JobBackend, request: job_objects.JobRequest) -> bool | sanitize.EdgeError:
    """Whether the backend permits the job, or the failure its check raised."""
    try:
        return backend.permits_job(request)
    except Exception as exc:
        return _edge(exc)


class JobTable:
    """Unreleased jobs, sessions being released, and the FIFO of work for the backend thread.

    ``write(frame, originated)`` sends one frame; ``originated`` asks the
    writer to number it as a message of the sidecar's own. ``note_error``
    takes the message of an error a backend raised, for health.
    """

    def __init__(
        self,
        write: Callable[[codec.SidecarFrame, bool], None],
        note_error: Callable[[str], None],
    ) -> None:
        self._write = write
        self._note_error = note_error
        self._lock = threading.Lock()
        self._work = threading.Condition(self._lock)
        self._lane: deque[Job | codec.SidecarFrame] = deque()
        self._jobs: dict[int, Job] = {}
        self._running: Job | None = None
        self._releasing: set[tuple[int, int]] = set()
        self._backend: JobBackend | None = None
        self._classes: tuple[str, ...] = ()
        self._closing: set[int] = set()
        self._stopped = False
        #: Whether a load ever enabled job messages on this connection.
        self.ever_opened = False

    def open(self, backend: JobBackend, classes: tuple[str, ...]) -> None:
        """Enable job messages for a loaded backend that runs ``classes``.

        Not when a request that unloads or replaces it already waits.
        """
        with self._lock:
            self.ever_opened = True
            if not self._closing:
                self._backend, self._classes = backend, classes

    def offer(self, frame: codec.SidecarFrame, closing: bool = False) -> bool:
        """Queue a unary request behind the work already waiting; False when too many wait.

        A ``closing`` request replaces or unloads the backend, so job messages
        are disabled first: waiting jobs fail, the running one ends as usual.
        """
        with self._lock:
            waiting = sum(isinstance(work, codec.SidecarFrame) for work in self._lane)
            if waiting >= UNARY_QUEUE_DEPTH:
                return False
            if closing:
                self._closing.add(id(frame))
                self._backend = None
                for job in [work for work in self._lane if isinstance(work, Job)]:
                    self._lane.remove(job)
                    self._end(job, protocol.ERR_UNAVAILABLE, "backend_unavailable")
            self._lane.append(frame)
            self._work.notify()
            return True

    def take(self) -> Job | codec.SidecarFrame | None:
        """Block until work is due; None once the lane was stopped and holds nothing."""
        with self._work:
            while not self._lane and not self._stopped:
                self._work.wait()
            if not self._lane:
                return None
            work = self._lane.popleft()
            self._closing.discard(id(work))
            if isinstance(work, Job):
                self._running = work
            return work

    def stop(self, *, drain: bool = False) -> None:
        """End the lane, dropping what waits unless the backend thread is to drain it."""
        with self._lock:
            self._stopped = True
            if not drain:
                self._lane.clear()
            self._work.notify_all()

    def handle(self, frame: codec.SidecarFrame) -> bool:
        """Act on a job or session message; False when no load enabled them.

        Behind a request that unloads or replaces the backend, a cancel or a
        session release still reaches the job that is running.
        """
        header, kind = frame.header, frame.header["kind"]
        with self._lock:
            backend = self._backend
            if backend is None and (kind == protocol.KIND_JOB_SUBMIT or not self._jobs):
                return False
            try:
                if backend is not None and kind == protocol.KIND_JOB_SUBMIT:
                    self._submit(frame, backend)
                elif kind == protocol.KIND_JOB_CANCEL:
                    identity = job_objects.read_cancel(header)
                    job = self._jobs.get(identity.job_id)
                    if job is not None and job.identity != identity:
                        self._refuse(header, "identity_mismatch")
                    elif job is not None:
                        self._cancel(job)
                else:
                    self._release_session(job_objects.read_session_release(header))
            except (job_objects.MalformedJob, job_objects.JobRefused) as exc:
                self._refuse(header, getattr(exc, "reason", None))
            return True

    def run(self, job: Job) -> None:
        """Run a job taken from the lane on the calling thread, then end and release it."""
        request, backend = job.request, job.backend
        result: object = None
        failure = None
        try:
            if request is None or backend is None:
                raise BackendError(protocol.ERR_INTERNAL, "the job was released before it ran")
            result = backend.run_job(request)
        except Exception as exc:
            failure = _edge(exc)
        with self._lock:
            self._running = None
            if job.cancelled:
                self._end(job, protocol.ERR_CANCELLED)
                return
            if failure is None:
                try:
                    self._emit(job, protocol.KIND_JOB_COMPLETED, result=result)
                except job_objects.JobRefused as exc:
                    self._end(job, protocol.ERR_INFERENCE_FAILED, exc.reason)
                except Exception as exc:
                    failure = _edge(exc)
                else:
                    self._release(job)
            if failure is not None:
                # Health has it before the job_failed that may prompt a look.
                self._note_error(failure.message)
                self._end(job, failure.code, None, failure.message)

    def _submit(self, frame: codec.SidecarFrame, backend: JobBackend) -> None:
        header = frame.header
        request = refusal = None
        try:
            request = job_objects.read_submit(header, frame.payload)
        except job_objects.JobRefused as exc:
            refusal = exc.reason
        identity = job_objects.read_identity(header)
        if request is None:
            job = Job(identity, job_objects.JobEventSequence(identity))
        else:
            sequence = job_objects.JobEventSequence.for_request(request)
            job = Job(identity, sequence, request, backend)
        if identity.job_id in self._jobs:
            # Not job messages: they would land in the live job's stream.
            self._refuse(header, "duplicate_job_id")
        elif request is None:
            self._end(job, protocol.ERR_CONFIG_INVALID, refusal)
        elif request.job_class not in self._classes:
            self._end(job, protocol.ERR_UNSUPPORTED, "job_class_unsupported")
        elif isinstance(permitted := _permits(backend, request), sanitize.EdgeError):
            self._note_error(permitted.message)
            self._end(job, permitted.code, None, permitted.message)
        elif not permitted:
            self._end(job, protocol.ERR_UNSUPPORTED, "job_not_permitted")
        elif job.session in self._releasing:
            self._end(job, protocol.ERR_NOT_READY, "session_releasing")
        elif sum(isinstance(work, Job) for work in self._lane) >= GPU_LANE_QUEUE_DEPTH:
            self._end(job, protocol.ERR_RESOURCE_EXHAUSTED, "job_capacity_exhausted")
        else:
            self._jobs[identity.job_id] = job
            self._emit(job, protocol.KIND_JOB_ACCEPTED)
            self._lane.append(job)
            self._work.notify()

    def _cancel(self, job: Job) -> None:
        if job.cancelled:
            return
        job.cancelled = True
        job.sequence.note_cancel_requested()
        self._emit(job, protocol.KIND_JOB_CANCEL_ACKNOWLEDGED)
        # The running job ends when its backend call returns; a waiting one ends now.
        if job is not self._running:
            self._lane.remove(job)
            self._end(job, protocol.ERR_CANCELLED)

    def _release_session(self, session: tuple[int, int]) -> None:
        # A repeat while the release is pending finds every job cancelled already.
        self._releasing.add(session)
        for job in [job for job in self._jobs.values() if job.session == session]:
            self._cancel(job)
        self._settle(session)

    def _settle(self, session: tuple[int, int]) -> None:
        """Send ``session_released`` and forget the session once none of its jobs is left."""
        if session not in self._releasing or any(
            job.session == session for job in self._jobs.values()
        ):
            return
        self._releasing.discard(session)
        key, generation = session
        released = _message(protocol.KIND_SESSION_RELEASED, session_key=key, generation=generation)
        self._write(released, True)

    def _emit(
        self, job: Job, kind: str, result: object = None, error: dict[str, str] | None = None
    ) -> None:
        header, payload = job_objects.job_event(kind, job.identity, result=result, error=error)
        job.sequence.observe(header, payload)
        self._write(codec.SidecarFrame(header, payload), True)

    def _end(
        self, job: Job, code: str, context: str | None = None, message: str | None = None
    ) -> None:
        """Send ``job_failed`` for ``job`` and release it."""
        fixed = sanitize.MESSAGES[code]
        try:
            error = job_objects.error_object(code, message or fixed, context)
        except job_objects.JobRefused:
            error = job_objects.error_object(code, fixed, context)
        self._emit(job, protocol.KIND_JOB_FAILED, error=error)
        self._release(job)

    def _release(self, job: Job) -> None:
        # The table lets go of the job and its buffers before it says so.
        self._jobs.pop(job.identity.job_id, None)
        job.request = job.backend = None
        self._emit(job, protocol.KIND_JOB_RELEASED)
        self._settle(job.session)

    def _refuse(self, header: dict[str, Any], context: str | None) -> None:
        """Answer a message the table cannot act on with an ``error_event``."""
        code, status = protocol.ERR_CONFIG_INVALID, protocol.STATUS_ERROR
        error = job_objects.error_object(code, sanitize.MESSAGES[code], context)
        kind = protocol.KIND_ERROR_EVENT
        self._write(_message(kind, header["message_id"], status=status, error=error), False)


__all__ = ["GPU_LANE_QUEUE_DEPTH", "LANE_JOB_CLASSES", "UNARY_QUEUE_DEPTH", "Job", "JobTable"]

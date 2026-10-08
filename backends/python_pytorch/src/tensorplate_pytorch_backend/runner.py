"""Sidecar runner: connect to the C++ adapter, dispatch IPC messages.

The runner is the entry point of the ``tensorplate-backend-python-pytorch``
process. It connects to a Unix domain socket whose path is supplied by
the adapter, reads sidecar frames, dispatches each one to a
:class:`Backend` or to the job table, and serializes what answers it. It
does not start an HTTP server, does not run a FastAPI app, and does not
load arbitrary user Python plugins outside the declared backend contract
(per the V01-E05 closed decisions).

Threads
    ``serve_forever`` starts a reader thread and makes every backend call on
    the thread that called it. The reader answers ``health_check`` itself
    and gives job and session messages to the job table
    (:mod:`~tensorplate_pytorch_backend.jobs`), which admits, cancels and
    releases without waiting for the backend. Every other frame and every
    admitted job waits in one FIFO for the calling thread, so backend calls
    never overlap and run in arrival order. At most 8 jobs and 8 unary
    requests wait; one more is refused with ``resource_exhausted``.

Writes
    A frame is written whole, under one lock. A write that makes no progress
    for ``WRITE_STALL_TIMEOUT_S`` ends the connection, as EOF or a frame
    error does: the socket is shut down, waiting work is dropped unanswered,
    and ``serve_forever`` returns when the backend call in progress has.

Lifecycle
    The runner owns at most one active backend at a time (one
    execution session per process is the V01-E05 invariant). The
    backend is constructed lazily when ``load_model`` arrives.

Failure handling
    Every backend-raised :class:`BackendError` becomes a typed
    ``*_response`` frame with ``status: "error"``. Unknown exceptions
    are caught at the dispatch boundary and converted to
    ``Error::Code::Internal`` (``oom_error`` for out-of-memory classes).
    Both pass through :mod:`~tensorplate_pytorch_backend.sanitize`, so no
    exception's text reaches the frame, health or the log. The runner
    never lets a backend exception kill the process.
"""

from __future__ import annotations

import argparse
import contextlib
import logging
import os
import select
import socket
import threading
import time
import uuid
from dataclasses import dataclass, field
from typing import Any, Final

from tensorplate_pytorch_backend import codec, jobs, protocol, sanitize
from tensorplate_pytorch_backend.backends import (
    Backend,
    BackendError,
    CudaFixtureBackend,
    FasterWhisperBackend,
    FixtureBackend,
    JobBackend,
    KokoroBackend,
    MpsFixtureBackend,
    NamedTensor,
    RuntimeCapability,
    SmolVLABackend,
)
from tensorplate_pytorch_backend.configuration import ArtifactConfigError, read_artifact_config

logger = logging.getLogger("tensorplate.sidecar")

#: The protocol capabilities the ``ready_event`` lists.
CAPABILITIES: Final[tuple[str, ...]] = (protocol.CAPABILITY_SPEECH_JOBS_V1,)
#: How long a write may make no progress before the connection is given up.
WRITE_STALL_TIMEOUT_S: float = 5.0
_JOB_KINDS: Final[frozenset[str]] = frozenset(
    {protocol.KIND_JOB_SUBMIT, protocol.KIND_JOB_CANCEL, protocol.KIND_SESSION_RELEASE}
)


# Registry of backend implementations selectable by `model_spec.backend_hint`
# inner discriminator. The `python_pytorch` backend_hint is the only one
# the adapter forwards; what discriminates between fixture / torchscript /
# smolvla is the `model_class` and a sidecar-specific config field. For
# V01-E05-F04 keeps the fixture as the default; SmolVLA is opt-in so host
# CI stays dependency-free while Jetson validation can exercise a real VLA.
def default_backend_factories() -> dict[str, type[Backend]]:
    return {
        "fixture": FixtureBackend,
        "cuda_fixture": CudaFixtureBackend,
        "mps_fixture": MpsFixtureBackend,
        "smolvla": SmolVLABackend,
        "faster_whisper": FasterWhisperBackend,
        "kokoro": KokoroBackend,
    }


@dataclass(slots=True)
class RunnerState:
    backend: Backend | None = None
    backend_factory_name: str | None = None
    last_error: str | None = None
    started_monotonic_ns: int = field(default_factory=time.monotonic_ns)
    async_seq: int = 0
    cancelled: set[str] = field(default_factory=set)


def _build_response_header(
    request: dict[str, Any], *, status: str, kind: str | None = None
) -> dict[str, Any]:
    response_kind = kind or protocol.REQUEST_TO_RESPONSE.get(request.get("kind", ""))
    if response_kind is None:
        # Unknown request kind. Surface it as an `error_event` so the
        # adapter still sees a structured error frame rather than a
        # dropped connection.
        response_kind = protocol.KIND_ERROR_EVENT
    header: dict[str, Any] = {
        "schema_version": protocol.SCHEMA_VERSION,
        "message_id": request.get("message_id") or uuid.uuid4().hex,
        "kind": response_kind,
        "status": status,
    }
    if "correlation_id" in request:
        header["correlation_id"] = request["correlation_id"]
    return header


def _typed_error_response(
    request: dict[str, Any],
    error: sanitize.EdgeError,
    *,
    runtime_capability: RuntimeCapability | None = None,
) -> codec.SidecarFrame:
    header = _build_response_header(request, status=protocol.STATUS_ERROR)
    header["error"] = {
        "schema_version": protocol.SCHEMA_VERSION,
        "code": error.code,
        "message": error.message,
    }
    if runtime_capability is not None:
        header["runtime_capability"] = runtime_capability.to_wire()
    return codec.SidecarFrame(header=header)


def _slice_tensors(frame: codec.SidecarFrame) -> list[NamedTensor]:
    items = frame.header.get("tensors") or []
    if not isinstance(items, list):
        raise BackendError(protocol.ERR_CONFIG_INVALID, "`tensors` must be an array")
    out: list[NamedTensor] = []
    for entry in items:
        if not isinstance(entry, dict):
            raise BackendError(protocol.ERR_CONFIG_INVALID, "each tensor entry must be an object")
        name = entry.get("name")
        tensor = entry.get("tensor")
        offset = entry.get("payload_offset")
        length = entry.get("payload_length")
        if not isinstance(name, str) or not name:
            raise BackendError(protocol.ERR_CONFIG_INVALID, "tensor.name is required")
        if not isinstance(tensor, dict):
            raise BackendError(protocol.ERR_CONFIG_INVALID, "tensor.tensor must be an object")
        if not isinstance(offset, int) or offset < 0:
            raise BackendError(
                protocol.ERR_CONFIG_INVALID, "tensor.payload_offset must be a non-negative integer"
            )
        if not isinstance(length, int) or length < 0:
            raise BackendError(
                protocol.ERR_CONFIG_INVALID, "tensor.payload_length must be a non-negative integer"
            )
        if offset + length > len(frame.payload):
            raise BackendError(
                protocol.ERR_SHAPE_MISMATCH, "a tensor's payload window exceeds the frame payload"
            )
        payload_slice = frame.payload[offset : offset + length]
        out.append(NamedTensor(name=name, tensor=tensor, payload=payload_slice))
    return out


def _pack_outputs(
    request: dict[str, Any], outputs: list[NamedTensor], *, kind: str
) -> codec.SidecarFrame:
    header = _build_response_header(request, status=protocol.STATUS_OK, kind=kind)
    tensors_meta: list[dict[str, Any]] = []
    payload = bytearray()
    for out in outputs:
        offset = len(payload)
        length = len(out.payload)
        tensors_meta.append(
            {
                "name": out.name,
                "tensor": out.tensor,
                "payload_offset": offset,
                "payload_length": length,
            }
        )
        payload.extend(out.payload)
    header["tensors"] = tensors_meta
    return codec.SidecarFrame(header=header, payload=bytes(payload))


class SidecarRunner:
    """Connects to one C++ adapter and serves one execution session.

    A reader thread and the thread that calls ``serve_forever`` share the
    work as the module docstring describes.
    """

    def __init__(
        self,
        sock: socket.socket,
        *,
        backend_factories: dict[str, type[Backend]] | None = None,
        default_backend_name: str = "fixture",
    ) -> None:
        self._sock = sock
        self._factories = backend_factories or default_backend_factories()
        self._default_backend_name = default_backend_name
        self._state = RunnerState()
        self._read_buf = bytearray()
        self._write_lock = threading.Lock()
        self._originated = 0
        self._ended = False
        self._table = jobs.JobTable(
            self._write_frame, lambda message: setattr(self._state, "last_error", message)
        )

    @property
    def state(self) -> RunnerState:
        return self._state

    def announce_ready(self) -> bool:
        """Send the ``ready_event`` with the sidecar's capabilities; False when it failed."""
        header = {
            "schema_version": protocol.SCHEMA_VERSION,
            "message_id": "",
            "kind": protocol.KIND_READY_EVENT,
            "capabilities": list(CAPABILITIES),
        }
        self._write_frame(codec.SidecarFrame(header), True)
        return not self._ended

    def serve_forever(self, *, max_iterations: int | None = None) -> None:
        reader = threading.Thread(
            target=self._read_frames, args=(max_iterations,), name="sidecar-reader", daemon=True
        )
        reader.start()
        try:
            while (work := self._table.take()) is not None:
                if isinstance(work, jobs.Job):
                    self._table.run(work)
                elif (response := self._dispatch(work)) is not None:
                    self._write_frame(response)
        except Exception as exc:
            logger.error("sidecar runner exiting on unexpected error: %s", sanitize.describe(exc))
            self._end_connection()
        reader.join(timeout=2 * WRITE_STALL_TIMEOUT_S)

    def _read_frames(self, limit: int | None) -> None:
        """The reader thread: route frames until EOF, an error or ``limit`` frames."""
        count = 0
        try:
            while limit is None or count < limit:
                frame = self._read_one_frame()
                if frame is None:
                    break  # peer closed
                count += 1
                self._route(frame)
        except (ConnectionError, OSError) as exc:
            logger.warning("sidecar runner exiting on socket error: %s", sanitize.describe(exc))
        except Exception as exc:
            logger.error("sidecar runner exiting on unexpected error: %s", sanitize.describe(exc))
        finally:
            # At the frame limit the backend thread still answers what was read;
            # otherwise the connection is over and what waits is dropped unanswered.
            if count != limit:
                self._end_connection()
            self._table.stop(drain=count == limit)

    def _route(self, frame: codec.SidecarFrame) -> None:
        kind = frame.header.get("kind")
        try:
            self._reject_bad_schema(frame.header)
        except BackendError:
            kind = None  # the backend thread answers it, in arrival order
        if kind == protocol.KIND_HEALTH_CHECK:
            response = self._dispatch(frame)
        elif kind in _JOB_KINDS and self._table.handle(frame):
            return
        else:
            if self._table.offer(frame, kind in (protocol.KIND_LOAD_MODEL, protocol.KIND_UNLOAD)):
                return
            code = protocol.ERR_RESOURCE_EXHAUSTED
            busy = sanitize.EdgeError(code, sanitize.MESSAGES[code])
            response = _typed_error_response(frame.header, busy)
        if response is not None:
            self._write_frame(response)

    # ------------------------------------------------------------------
    # framing
    # ------------------------------------------------------------------

    def _read_one_frame(self) -> codec.SidecarFrame | None:
        while True:
            try:
                frame, consumed = codec.decode_one(bytes(self._read_buf))
            except codec.IncompleteFrame:
                chunk = self._sock.recv(65536)
                if not chunk:
                    return None
                self._read_buf.extend(chunk)
                continue
            del self._read_buf[:consumed]
            return frame

    def _write_frame(self, frame: codec.SidecarFrame, originated: bool = False) -> None:
        """Write one whole frame; a write that fails or stalls ends the connection.

        An ``originated`` frame gets the connection's next ``s<n>`` as its ``message_id``.
        """
        with self._write_lock:
            if self._ended:
                return
            if originated:
                self._originated += 1
                frame.header["message_id"] = f"s{self._originated}"
            try:
                view = memoryview(codec.encode(frame))
                while view:
                    try:
                        view = view[self._sock.send(view, socket.MSG_DONTWAIT) :]
                    except BlockingIOError:
                        # Not a socket timeout, which would bound reads as well.
                        ready = select.select([], [self._sock], [], WRITE_STALL_TIMEOUT_S)
                        if not ready[1]:
                            raise TimeoutError from None
            except OSError as exc:
                logger.warning(
                    "sidecar runner exiting on socket write error: %s", sanitize.describe(exc)
                )
                self._end_connection()

    def _end_connection(self) -> None:
        self._ended = True
        with contextlib.suppress(OSError):
            self._sock.shutdown(socket.SHUT_RDWR)

    # ------------------------------------------------------------------
    # dispatch
    # ------------------------------------------------------------------

    def _dispatch(self, frame: codec.SidecarFrame) -> codec.SidecarFrame | None:
        header = frame.header
        try:
            self._reject_bad_schema(header)
            kind = header.get("kind")
            if kind == protocol.KIND_LOAD_MODEL:
                return self._handle_load(frame)
            if kind == protocol.KIND_PRIME:
                return self._handle_prime(frame)
            if kind == protocol.KIND_INFER:
                return self._handle_infer(frame, async_dispatch=False)
            if kind == protocol.KIND_INFER_ASYNC:
                return self._handle_infer(frame, async_dispatch=True)
            if kind == protocol.KIND_CANCEL:
                return self._handle_cancel(frame)
            if kind == protocol.KIND_UNLOAD:
                return self._handle_unload(frame)
            if kind == protocol.KIND_HEALTH_CHECK:
                return self._handle_health_check(frame)
            raise BackendError(protocol.ERR_UNSUPPORTED, "unknown or unsupported message kind")
        except Exception as exc:
            if not isinstance(exc, BackendError):
                logger.error("sidecar request failed: %s", sanitize.describe(exc))
            error = sanitize.edge_error(exc)
            self._state.last_error = error.message
            capability = exc.runtime_capability if isinstance(exc, BackendError) else None
            return _typed_error_response(header, error, runtime_capability=capability)

    def _reject_bad_schema(self, header: dict[str, Any]) -> None:
        if header.get("schema_version") != protocol.SCHEMA_VERSION:
            raise BackendError(protocol.ERR_UNSUPPORTED, "unsupported schema_version")
        if not isinstance(header.get("message_id"), str) or not header["message_id"]:
            raise BackendError(protocol.ERR_CONFIG_INVALID, "message_id is required")
        if not isinstance(header.get("kind"), str) or not header["kind"]:
            raise BackendError(protocol.ERR_CONFIG_INVALID, "kind is required")

    # ------------------------------------------------------------------
    # handlers
    # ------------------------------------------------------------------

    def _ensure_backend(self) -> Backend:
        if self._state.backend is None:
            raise BackendError(protocol.ERR_NOT_READY, "no backend loaded")
        return self._state.backend

    def _resolve_factory(self, model_spec: dict[str, Any]) -> type[Backend]:
        # The sidecar protocol carries a ModelSpec object on load. The
        # `backend_hint` on that ModelSpec is always `python_pytorch`
        # (the C++ adapter only forwards python_pytorch bundles), so we
        # discriminate by an optional `profile_id`, a sidecar-private
        # artifact setting, or the default factory.
        profile_id = model_spec.get("profile_id")
        if profile_id is not None and (not isinstance(profile_id, str) or not profile_id):
            raise BackendError(
                protocol.ERR_CONFIG_INVALID,
                "model_spec.profile_id must be a non-empty string when present",
            )
        try:
            configured_name = read_artifact_config(model_spec).get("backend_profile")
        except ArtifactConfigError as exc:
            raise BackendError(protocol.ERR_CONFIG_INVALID, str(exc)) from exc
        if configured_name is not None and (
            not isinstance(configured_name, str) or not configured_name
        ):
            raise BackendError(
                protocol.ERR_CONFIG_INVALID,
                "sidecar config backend_profile must be a non-empty string when present",
            )
        if profile_id and configured_name and profile_id != configured_name:
            raise BackendError(
                protocol.ERR_CONFIG_INVALID,
                "model_spec.profile_id conflicts with sidecar config backend_profile",
            )
        requested_name = profile_id or configured_name or self._default_backend_name
        if requested_name in self._factories:
            return self._factories[requested_name]
        raise BackendError(
            protocol.ERR_CONFIG_INVALID,
            "no sidecar backend is registered for the requested profile",
        )

    def _handle_load(self, frame: codec.SidecarFrame) -> codec.SidecarFrame:
        model_spec = frame.header.get("model_spec")
        if not isinstance(model_spec, dict):
            raise BackendError(protocol.ERR_CONFIG_INVALID, "load_model requires model_spec")
        enabled = frame.header.get("capabilities", [])
        if not isinstance(enabled, list) or any(item not in CAPABILITIES for item in enabled):
            raise BackendError(
                protocol.ERR_UNSUPPORTED, "load_model enables a capability the sidecar did not list"
            )
        with_jobs = protocol.CAPABILITY_SPEECH_JOBS_V1 in enabled
        factory_cls = self._resolve_factory(model_spec)
        backend = factory_cls()
        job_backend = backend if with_jobs and isinstance(backend, JobBackend) else None
        if with_jobs and job_backend is None:
            raise BackendError(protocol.ERR_UNSUPPORTED, "the selected runner does not run jobs")
        backend.load(model_spec)
        classes: tuple[str, ...] = ()
        if job_backend is not None:
            declared = job_backend.job_classes()
            classes = tuple(name for name in jobs.LANE_JOB_CLASSES if name in declared)
            if not classes:
                backend.unload()
                raise BackendError(
                    protocol.ERR_UNSUPPORTED, "the loaded runner runs no job class that has a lane"
                )
        self._state.backend = backend
        self._state.backend_factory_name = backend.name
        header = _build_response_header(frame.header, status=protocol.STATUS_OK)
        if job_backend is not None:
            self._table.open(job_backend, classes)
            header["job_classes"] = list(classes)
        if backend.runtime_capability is not None:
            header["runtime_capability"] = backend.runtime_capability.to_wire()
        return codec.SidecarFrame(header=header)

    def _handle_prime(self, frame: codec.SidecarFrame) -> codec.SidecarFrame:
        backend = self._ensure_backend()
        backend.prime()
        return codec.SidecarFrame(
            header=_build_response_header(frame.header, status=protocol.STATUS_OK)
        )

    def _handle_infer(
        self, frame: codec.SidecarFrame, *, async_dispatch: bool
    ) -> codec.SidecarFrame:
        backend = self._ensure_backend()
        correlation_id = frame.header.get("correlation_id")
        if isinstance(correlation_id, str) and correlation_id in self._state.cancelled:
            self._state.cancelled.discard(correlation_id)
            raise BackendError(protocol.ERR_TIMEOUT, "request was cancelled before dispatch")
        inputs = _slice_tensors(frame)
        if async_dispatch:
            self._state.async_seq += 1
            outputs = backend.infer_async(inputs)
            response = _pack_outputs(frame.header, outputs, kind=protocol.KIND_INFER_ASYNC_RESPONSE)
            response.header["async_id"] = self._state.async_seq
            return response
        outputs = backend.infer(inputs)
        return _pack_outputs(frame.header, outputs, kind=protocol.KIND_INFER_RESPONSE)

    def _handle_cancel(self, frame: codec.SidecarFrame) -> codec.SidecarFrame:
        correlation_id = frame.header.get("correlation_id")
        if isinstance(correlation_id, str) and correlation_id:
            self._state.cancelled.add(correlation_id)
            if self._state.backend is not None:
                self._state.backend.cancel(correlation_id)
        return codec.SidecarFrame(
            header=_build_response_header(frame.header, status=protocol.STATUS_OK)
        )

    def _handle_unload(self, frame: codec.SidecarFrame) -> codec.SidecarFrame:
        if self._state.backend is not None:
            self._state.backend.unload()
        self._state.backend = None
        return codec.SidecarFrame(
            header=_build_response_header(frame.header, status=protocol.STATUS_OK)
        )

    def _handle_health_check(self, frame: codec.SidecarFrame) -> codec.SidecarFrame:
        header = _build_response_header(frame.header, status=protocol.STATUS_OK)
        backend = self._state.backend  # read once: the backend thread may replace it
        header["health"] = {
            "ready": backend is not None,
            "backend_factory": self._state.backend_factory_name,
            "uptime_ns": time.monotonic_ns() - self._state.started_monotonic_ns,
            "last_error": self._state.last_error,
        }
        if backend is not None and backend.runtime_capability is not None:
            header["runtime_capability"] = backend.runtime_capability.to_wire()
        return codec.SidecarFrame(header=header)


# ----------------------------------------------------------------------
# CLI / process entry point
# ----------------------------------------------------------------------


def _connect_socket(socket_path: str, *, timeout_s: float) -> socket.socket:
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.settimeout(timeout_s)
    sock.connect(socket_path)
    sock.settimeout(None)
    return sock


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="tensorplate-backend-python-pytorch",
        description="TensorPlate Python/PyTorch sidecar backend runner.",
    )
    parser.add_argument(
        "--socket",
        required=True,
        help="Path to the Unix-domain socket created by the C++ adapter.",
    )
    parser.add_argument(
        "--connect-timeout-s",
        type=float,
        default=5.0,
        help="Connect timeout (seconds) before giving up on the adapter.",
    )
    parser.add_argument(
        "--default-backend",
        default=os.environ.get("TP_PYTHON_PYTORCH_DEFAULT_BACKEND", "fixture"),
        help="Backend factory used when the bundle does not declare a profile_id.",
    )
    parser.add_argument(
        "--log-level",
        default=os.environ.get("TP_SIDECAR_LOG_LEVEL", "WARNING"),
    )
    args = parser.parse_args(argv)

    sanitize.configure_process_logging(args.log_level)

    try:
        sock = _connect_socket(args.socket, timeout_s=args.connect_timeout_s)
    except OSError as exc:
        logger.error("tensorplate sidecar: connect failed: %s", sanitize.describe(exc))
        return 2

    runner = SidecarRunner(sock, default_backend_name=args.default_backend)

    # Emit a `ready_event` so the adapter can transition to ready
    # without polling health_check.
    if not runner.announce_ready():
        sock.close()
        return 3

    runner.serve_forever()
    sock.close()
    return 0


__all__ = [
    "RunnerState",
    "SidecarRunner",
    "default_backend_factories",
    "main",
]

"""Sidecar runner: connect to the C++ adapter, dispatch IPC messages.

The runner is the entry point of the ``tensorplate-backend-python-pytorch``
process. It connects to a Unix domain socket whose path is supplied by
the adapter, reads sidecar frames in a loop, dispatches each frame to a
:class:`Backend`, and serializes the response frame. It does not start
an HTTP server, does not run a FastAPI app, and does not load arbitrary
user Python plugins outside the declared backend contract (per the
V01-E05 closed decisions).

Lifecycle
    The runner owns at most one active backend at a time (one
    execution session per process is the V01-E05 invariant). The
    backend is constructed lazily when ``load_model`` arrives.

Failure handling
    Every backend-raised :class:`BackendError` becomes a typed
    ``*_response`` frame with ``status: "error"``. Unknown exceptions
    are caught at the dispatch boundary and converted to
    ``Error::Code::Internal``. The runner never lets a backend
    exception kill the process.
"""

from __future__ import annotations

import argparse
import logging
import os
import socket
import sys
import threading
import time
import uuid
from dataclasses import dataclass, field
from typing import Any

from tensorplate_pytorch_backend import codec, protocol
from tensorplate_pytorch_backend.backends import (
    Backend,
    BackendError,
    FixtureBackend,
    NamedTensor,
    RuntimeCapability,
    SmolVLABackend,
)

logger = logging.getLogger("tensorplate.sidecar")


# Registry of backend implementations selectable by `model_spec.backend_hint`
# inner discriminator. The `python_pytorch` backend_hint is the only one
# the adapter forwards; what discriminates between fixture / torchscript /
# smolvla is the `model_class` and a sidecar-specific config field. For
# V01-E05-F04 keeps the fixture as the default; SmolVLA is opt-in so host
# CI stays dependency-free while Jetson validation can exercise a real VLA.
def default_backend_factories() -> dict[str, type[Backend]]:
    return {"fixture": FixtureBackend, "smolvla": SmolVLABackend}


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
    code: str,
    message: str,
    *,
    context: str | None = None,
    runtime_capability: RuntimeCapability | None = None,
) -> codec.SidecarFrame:
    header = _build_response_header(request, status=protocol.STATUS_ERROR)
    error: dict[str, Any] = {"code": code, "message": message}
    if context is not None:
        error["context"] = context
    header["error"] = error
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
                protocol.ERR_SHAPE_MISMATCH,
                f"tensor `{name}` payload window [{offset},{offset + length}) "
                f"exceeds frame payload size {len(frame.payload)}",
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

    The runner is a synchronous request/response loop; sidecar IPC
    serialization is the adapter's responsibility on the C++ side.
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

    @property
    def state(self) -> RunnerState:
        return self._state

    def serve_forever(self, *, max_iterations: int | None = None) -> None:
        iterations = 0
        try:
            while True:
                if max_iterations is not None and iterations >= max_iterations:
                    return
                iterations += 1
                frame = self._read_one_frame()
                if frame is None:
                    return  # peer closed
                response = self._dispatch(frame)
                if response is not None:
                    try:
                        self._write_frame(response)
                    except (ConnectionError, OSError) as exc:
                        logger.warning("sidecar runner exiting on socket write error: %s", exc)
                        return
        except (ConnectionError, OSError) as exc:
            logger.warning("sidecar runner exiting on socket error: %s", exc)
        except Exception:
            logger.exception("sidecar runner exiting on unexpected error")

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

    def _write_frame(self, frame: codec.SidecarFrame) -> None:
        data = codec.encode(frame)
        with self._write_lock:
            self._sock.sendall(data)

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
            raise BackendError(
                protocol.ERR_UNSUPPORTED, f"unknown or unsupported message kind: {kind!r}"
            )
        except BackendError as err:
            self._state.last_error = err.code_message
            return _typed_error_response(
                header,
                err.code,
                err.code_message,
                context=err.context,
                runtime_capability=err.runtime_capability,
            )
        except Exception as exc:
            logger.exception("unexpected sidecar dispatch failure")
            self._state.last_error = str(exc)
            return _typed_error_response(header, protocol.ERR_INTERNAL, str(exc))

    def _reject_bad_schema(self, header: dict[str, Any]) -> None:
        if header.get("schema_version") != protocol.SCHEMA_VERSION:
            raise BackendError(
                protocol.ERR_UNSUPPORTED,
                f"unsupported schema_version {header.get('schema_version')!r}",
            )
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
        # discriminate by an optional `profile_id` or fall back to the
        # default factory. v0.1.0 ships only the fixture backend.
        profile_id = model_spec.get("profile_id")
        if profile_id and profile_id in self._factories:
            return self._factories[profile_id]
        if self._default_backend_name in self._factories:
            return self._factories[self._default_backend_name]
        raise BackendError(
            protocol.ERR_CONFIG_INVALID,
            f"no python_pytorch sidecar backend registered for profile_id={profile_id!r}",
        )

    def _handle_load(self, frame: codec.SidecarFrame) -> codec.SidecarFrame:
        model_spec = frame.header.get("model_spec")
        if not isinstance(model_spec, dict):
            raise BackendError(protocol.ERR_CONFIG_INVALID, "load_model requires model_spec")
        factory_cls = self._resolve_factory(model_spec)
        backend = factory_cls()
        backend.load(model_spec)
        self._state.backend = backend
        self._state.backend_factory_name = backend.name
        header = _build_response_header(frame.header, status=protocol.STATUS_OK)
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
        header["health"] = {
            "ready": self._state.backend is not None,
            "backend_factory": self._state.backend_factory_name,
            "uptime_ns": time.monotonic_ns() - self._state.started_monotonic_ns,
            "last_error": self._state.last_error,
        }
        if self._state.backend is not None and self._state.backend.runtime_capability is not None:
            header["runtime_capability"] = self._state.backend.runtime_capability.to_wire()
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

    logging.basicConfig(level=args.log_level)

    try:
        sock = _connect_socket(args.socket, timeout_s=args.connect_timeout_s)
    except OSError as exc:
        sys.stderr.write(f"tensorplate sidecar: connect failed: {exc}\n")
        return 2

    runner = SidecarRunner(sock, default_backend_name=args.default_backend)

    # Emit a `ready_event` so the adapter can transition to ready
    # without polling health_check.
    try:
        runner._write_frame(
            codec.SidecarFrame(
                header={
                    "schema_version": protocol.SCHEMA_VERSION,
                    "message_id": uuid.uuid4().hex,
                    "kind": protocol.KIND_READY_EVENT,
                }
            )
        )
    except OSError:
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

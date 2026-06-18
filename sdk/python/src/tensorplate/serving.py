"""Synchronous client for the TensorPlate v0.1 serving HTTP envelope."""

from __future__ import annotations

import json
import uuid
from collections.abc import Sequence
from dataclasses import dataclass

from tensorplate.client import (
    DEFAULT_TIMEOUT_S,
    ResolvedEndpoint,
    http_request,
    resolve_serving_url,
)
from tensorplate.errors import (
    ErrorCode,
    ProtocolError,
    ServingError,
    UnsupportedSchemaVersionError,
)
from tensorplate.tensors import DType, Layout, TensorInput, TensorOutput

#: Serving envelope schema_version this SDK speaks.
SCHEMA_VERSION = "0.1"

_HEALTH_PATH = "/health"


@dataclass(frozen=True)
class Timing:
    """Optional per-request timing reported by the worker."""

    queue_latency_ns: int | None = None
    execution_latency_ns: int | None = None
    total_latency_ns: int | None = None


@dataclass(frozen=True)
class InferResult:
    """Parsed v0.1 ``success`` response."""

    request_id: str
    outputs: tuple[TensorOutput, ...]
    correlation_id: str | None = None
    timing: Timing | None = None

    def output(self, name: str) -> TensorOutput:
        """Return the output tensor named ``name`` or raise ``KeyError``."""
        for tensor in self.outputs:
            if tensor.name == name:
                return tensor
        raise KeyError(f"no output named {name!r}")


@dataclass(frozen=True)
class HealthSnapshot:
    """Parsed ``GET /health`` response."""

    state: str
    endpoint: str
    backend: str
    active_model_id: str | None = None
    last_error_code: ErrorCode | None = None
    last_error_message: str | None = None
    queue_depth: int | None = None
    in_flight: int | None = None

    @property
    def is_ready(self) -> bool:
        """True only for the ``ready`` state (``degraded`` is not ready)."""
        return self.state == "ready"


class ServingClient:
    """Synchronous client for the v0.1 ``/infer`` serving HTTP envelope.

    The endpoint is resolved once at construction with the same precedence
    as ``tensorplate infer`` (explicit URL, then CLI profile, then
    read-only agent discovery, then the loopback default). The client is
    model-class-neutral so higher-level helpers can build on it.
    """

    def __init__(
        self,
        serving_url: str | None = None,
        *,
        profile: str | None = None,
        config_path: str | None = None,
        timeout: float = DEFAULT_TIMEOUT_S,
        discover: bool = True,
    ) -> None:
        self._endpoint = resolve_serving_url(
            serving_url,
            profile=profile,
            config_path=config_path,
            timeout=timeout,
            discover=discover,
        )
        self._timeout = timeout

    @property
    def endpoint(self) -> ResolvedEndpoint:
        """The resolved serving endpoint and how it was resolved."""
        return self._endpoint

    @staticmethod
    def tensor_input(
        name: str,
        data: bytes,
        dtype: DType | str,
        shape: Sequence[int],
        *,
        layout: Layout | str = Layout.ROW_MAJOR,
    ) -> TensorInput:
        """Build a :class:`TensorInput` from raw bytes and metadata."""
        return TensorInput(
            name=name,
            dtype=DType(dtype),
            shape=tuple(shape),
            data=data,
            layout=Layout(layout),
        )

    def infer(
        self,
        endpoint: str,
        inputs: Sequence[TensorInput],
        *,
        deadline_ms: int | None = None,
        correlation_id: str | None = None,
    ) -> InferResult:
        """Run a single synchronous inference against ``endpoint``.

        Raises :class:`~tensorplate.errors.ServingError` for a typed
        ``failure`` envelope, :class:`~tensorplate.errors.TransportError`
        / :class:`~tensorplate.errors.RequestTimeoutError` for transport
        failures, and :class:`~tensorplate.errors.ProtocolError` /
        :class:`~tensorplate.errors.UnsupportedSchemaVersionError` for a
        malformed or unsupported response.
        """
        if not endpoint:
            raise ValueError("infer endpoint must be non-empty")
        if not inputs:
            raise ValueError("infer requires at least one input tensor")
        if deadline_ms is not None and (
            not isinstance(deadline_ms, int) or isinstance(deadline_ms, bool) or deadline_ms < 1
        ):
            raise ValueError("deadline_ms must be an integer greater than or equal to 1")
        if correlation_id is not None and not correlation_id:
            raise ValueError("correlation_id must be non-empty when provided")
        request_id = str(uuid.uuid4())
        request_body: dict[str, object] = {
            "schema_version": SCHEMA_VERSION,
            "request_id": request_id,
            "endpoint": endpoint,
            "inputs": [tensor.to_named_input() for tensor in inputs],
        }
        if deadline_ms is not None:
            request_body["deadline_ms"] = deadline_ms
        headers: dict[str, str] = {}
        if correlation_id is not None:
            request_body["metadata"] = {"correlation_id": correlation_id}
            headers["X-Correlation-Id"] = correlation_id
        payload = json.dumps(request_body).encode("utf-8")
        status_code, raw = http_request(
            "POST", self._endpoint.url, body=payload, headers=headers, timeout=self._timeout
        )
        return _parse_infer_response(raw, status_code, self._endpoint.url)

    def health(self) -> HealthSnapshot:
        """Read ``GET /health`` and parse the worker's readiness snapshot.

        The body is parsed regardless of the HTTP status (``ready`` and
        ``degraded`` return 200; the rest return 503), so inspect
        :attr:`HealthSnapshot.state`, not just reachability.
        """
        url = f"{self._endpoint.origin}{_HEALTH_PATH}"
        _status_code, raw = http_request("GET", url, timeout=self._timeout)
        payload = _decode_json_object(raw, url)
        _require_supported_schema(payload)
        return _parse_health(payload, url)


def _decode_json_object(raw: bytes, url: str) -> dict[str, object]:
    try:
        parsed = json.loads(raw)
    except ValueError as exc:
        raise ProtocolError(f"serving response from {url!r} is not valid JSON: {exc}") from exc
    if not isinstance(parsed, dict):
        raise ProtocolError(f"serving response from {url!r} is not a JSON object")
    return parsed


def _require_supported_schema(payload: dict[str, object]) -> None:
    version = payload.get("schema_version")
    if version != SCHEMA_VERSION:
        raise UnsupportedSchemaVersionError(
            version if isinstance(version, str) else None, SCHEMA_VERSION
        )


def _parse_infer_response(raw: bytes, status_code: int, url: str) -> InferResult:
    payload = _decode_json_object(raw, url)
    _require_supported_schema(payload)
    status = payload.get("status")
    if status == "failure":
        raise _serving_error(payload)
    if status != "success":
        raise ProtocolError(
            f"serving response from {url!r} has unexpected status {status!r} (HTTP {status_code})"
        )
    outputs_obj = payload.get("outputs")
    if not isinstance(outputs_obj, list) or not outputs_obj:
        raise ProtocolError(f"serving success response from {url!r} is missing non-empty 'outputs'")
    outputs = tuple(TensorOutput.from_named_output(item) for item in outputs_obj)
    request_id = payload.get("request_id")
    if not isinstance(request_id, str):
        raise ProtocolError(f"serving success response from {url!r} is missing 'request_id'")
    correlation_id = payload.get("correlation_id")
    return InferResult(
        request_id=request_id,
        outputs=outputs,
        correlation_id=correlation_id if isinstance(correlation_id, str) else None,
        timing=_parse_timing(payload.get("timing")),
    )


def _serving_error(payload: dict[str, object]) -> ServingError:
    request_id = payload.get("request_id")
    request_id = request_id if isinstance(request_id, str) else None
    error_obj = payload.get("error")
    if not isinstance(error_obj, dict):
        return ServingError(
            ErrorCode.INTERNAL,
            "serving worker returned a failure without a typed error",
            request_id=request_id,
        )
    try:
        code = ErrorCode(error_obj.get("code"))
    except ValueError:
        code = ErrorCode.INTERNAL
    message_obj = error_obj.get("message")
    context_obj = error_obj.get("context")
    return ServingError(
        code,
        message_obj if isinstance(message_obj, str) else "serving worker returned an error",
        context=context_obj if isinstance(context_obj, str) else None,
        request_id=request_id,
    )


def _parse_health(payload: dict[str, object], url: str) -> HealthSnapshot:
    state = payload.get("state")
    if not isinstance(state, str):
        raise ProtocolError(f"/health response from {url!r} is missing 'state'")
    endpoint = payload.get("endpoint")
    backend = payload.get("backend")
    code_obj = payload.get("last_error_code")
    try:
        last_error_code = ErrorCode(code_obj) if code_obj is not None else None
    except ValueError:
        last_error_code = None
    return HealthSnapshot(
        state=state,
        endpoint=endpoint if isinstance(endpoint, str) else "",
        backend=backend if isinstance(backend, str) else "",
        active_model_id=_optional_str(payload.get("active_model_id")),
        last_error_code=last_error_code,
        last_error_message=_optional_str(payload.get("last_error_message")),
        queue_depth=_optional_int(payload.get("queue_depth")),
        in_flight=_optional_int(payload.get("in_flight")),
    )


def _parse_timing(obj: object) -> Timing | None:
    if not isinstance(obj, dict):
        return None
    return Timing(
        queue_latency_ns=_optional_int(obj.get("queue_latency_ns")),
        execution_latency_ns=_optional_int(obj.get("execution_latency_ns")),
        total_latency_ns=_optional_int(obj.get("total_latency_ns")),
    )


def _optional_int(value: object) -> int | None:
    return value if isinstance(value, int) else None


def _optional_str(value: object) -> str | None:
    return value if isinstance(value, str) else None

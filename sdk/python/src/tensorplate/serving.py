"""Synchronous client for the TensorPlate v0.1 serving HTTP envelope."""

from __future__ import annotations

import json
import math
import struct
import time
import uuid
from collections.abc import Sequence
from dataclasses import dataclass
from typing import Literal

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
from tensorplate.tensors import DType, Layout, TensorInput, TensorOutput, itemsize

#: Serving envelope schema_version this SDK speaks.
SCHEMA_VERSION = "0.1"

_HEALTH_PATH = "/health"
_BINARY_CONTENT_TYPE = "application/vnd.tensorplate.infer.binary.v1"
_BINARY_INFER_MAGIC = b"TPINFER1"
_BINARY_RESULT_MAGIC = b"TPRESULT1"
TransportPreference = Literal["auto", "binary", "json"]


@dataclass(frozen=True)
class Timing:
    """Optional per-request timing reported by the worker."""

    queue_latency_ns: int | None = None
    execution_latency_ns: int | None = None
    total_latency_ns: int | None = None


@dataclass(frozen=True)
class ClientTiming:
    """Optional SDK-side timing for request encode, transport, and decode."""

    encode_ns: int
    http_roundtrip_ns: int
    decode_ns: int
    request_bytes: int
    response_bytes: int


@dataclass(frozen=True)
class InferResult:
    """Parsed v0.1 ``success`` response."""

    request_id: str
    outputs: tuple[TensorOutput, ...]
    correlation_id: str | None = None
    timing: Timing | None = None
    transport: str = "json"
    client_timing: ClientTiming | None = None

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
        preferred_transport: TransportPreference = "auto",
    ) -> None:
        if preferred_transport not in ("auto", "binary", "json"):
            raise ValueError("preferred_transport must be 'auto', 'binary', or 'json'")
        self._endpoint = resolve_serving_url(
            serving_url,
            profile=profile,
            config_path=config_path,
            timeout=timeout,
            discover=discover,
        )
        self._timeout = timeout
        self._preferred_transport: TransportPreference = preferred_transport
        self._binary_supported: bool | None = None

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
        profile: bool = False,
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
        if self._should_try_binary():
            status_code, raw, timing = self._infer_http(
                "binary",
                request_id,
                endpoint,
                inputs,
                deadline_ms=deadline_ms,
                correlation_id=correlation_id,
                profile=profile,
            )
            if status_code == 415 and _is_binary_unsupported(raw):
                if self._preferred_transport == "binary":
                    return _parse_infer_response(
                        raw, status_code, self._endpoint.url, "binary", timing
                    )
                # In 'auto' mode a 415 'unsupported' is ambiguous: the worker may
                # lack the binary transport, or the request itself may be
                # unsupported (unknown dtype/layout, schema_version, ...) — both
                # surface as 415 + code 'unsupported'. Retry over JSON and only
                # mark binary unsupported when that retry actually succeeds;
                # otherwise the failure was request content, not transport
                # capability, so binary must stay enabled (a transient bad
                # request must not permanently downgrade the client to JSON).
                status_code, raw, timing = self._infer_http(
                    "json",
                    request_id,
                    endpoint,
                    inputs,
                    deadline_ms=deadline_ms,
                    correlation_id=correlation_id,
                    profile=profile,
                )
                result = _parse_infer_response(raw, status_code, self._endpoint.url, "json", timing)
                self._binary_supported = False
                return result
            else:
                result = _parse_binary_or_json_response(
                    raw, status_code, self._endpoint.url, "binary", timing
                )
                if result.transport == "binary":
                    self._binary_supported = True
                return result

        status_code, raw, timing = self._infer_http(
            "json",
            request_id,
            endpoint,
            inputs,
            deadline_ms=deadline_ms,
            correlation_id=correlation_id,
            profile=profile,
        )
        return _parse_infer_response(raw, status_code, self._endpoint.url, "json", timing)

    def _should_try_binary(self) -> bool:
        if self._preferred_transport == "binary":
            return True
        if self._preferred_transport == "json":
            return False
        return self._binary_supported is not False

    def _infer_http(
        self,
        transport: Literal["binary", "json"],
        request_id: str,
        endpoint: str,
        inputs: Sequence[TensorInput],
        *,
        deadline_ms: int | None,
        correlation_id: str | None,
        profile: bool,
    ) -> tuple[int, bytes, ClientTiming | None]:
        t0 = time.perf_counter_ns()
        if transport == "binary":
            payload = _encode_binary_infer_request(
                request_id,
                endpoint,
                inputs,
                deadline_ms=deadline_ms,
                correlation_id=correlation_id,
            )
            headers = {
                "Content-Type": _BINARY_CONTENT_TYPE,
                "Accept": f"{_BINARY_CONTENT_TYPE}, application/json",
            }
            if correlation_id is not None:
                headers["X-Correlation-Id"] = correlation_id
        else:
            payload, headers = _encode_json_infer_request(
                request_id,
                endpoint,
                inputs,
                deadline_ms=deadline_ms,
                correlation_id=correlation_id,
            )
        t1 = time.perf_counter_ns()
        status_code, raw = http_request(
            "POST", self._endpoint.url, body=payload, headers=headers, timeout=self._timeout
        )
        t2 = time.perf_counter_ns()
        timing = (
            ClientTiming(
                encode_ns=t1 - t0,
                http_roundtrip_ns=t2 - t1,
                decode_ns=0,
                request_bytes=len(payload),
                response_bytes=len(raw),
            )
            if profile
            else None
        )
        return status_code, raw, timing

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


def _encode_json_infer_request(
    request_id: str,
    endpoint: str,
    inputs: Sequence[TensorInput],
    *,
    deadline_ms: int | None = None,
    correlation_id: str | None = None,
) -> tuple[bytes, dict[str, str]]:
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
    return json.dumps(request_body).encode("utf-8"), headers


def _encode_binary_infer_request(
    request_id: str,
    endpoint: str,
    inputs: Sequence[TensorInput],
    *,
    deadline_ms: int | None = None,
    correlation_id: str | None = None,
) -> bytes:
    payload = bytearray()
    encoded_inputs: list[dict[str, object]] = []
    for tensor in inputs:
        offset = len(payload)
        payload.extend(tensor.data)
        encoded_inputs.append(
            {
                "name": tensor.name,
                "tensor": {
                    "dtype": tensor.dtype.value,
                    "layout": tensor.layout.value,
                    "shape": list(tensor.shape),
                    "byte_offset": 0,
                    "byte_size": len(tensor.data),
                },
                "payload_offset": offset,
                "payload_size": len(tensor.data),
            }
        )
    request_body: dict[str, object] = {
        "schema_version": SCHEMA_VERSION,
        "request_id": request_id,
        "endpoint": endpoint,
        "inputs": encoded_inputs,
    }
    if deadline_ms is not None:
        request_body["deadline_ms"] = deadline_ms
    if correlation_id is not None:
        request_body["metadata"] = {"correlation_id": correlation_id}
    metadata = json.dumps(request_body, separators=(",", ":")).encode("utf-8")
    if len(metadata) > 0xFFFFFFFF:
        raise ValueError("binary infer metadata exceeds uint32 length limit")
    return _BINARY_INFER_MAGIC + struct.pack("<I", len(metadata)) + metadata + bytes(payload)


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


def _with_decode_timing(
    result: InferResult, timing: ClientTiming | None, started_ns: int
) -> InferResult:
    if timing is None:
        return result
    return InferResult(
        request_id=result.request_id,
        outputs=result.outputs,
        correlation_id=result.correlation_id,
        timing=result.timing,
        transport=result.transport,
        client_timing=ClientTiming(
            encode_ns=timing.encode_ns,
            http_roundtrip_ns=timing.http_roundtrip_ns,
            decode_ns=time.perf_counter_ns() - started_ns,
            request_bytes=timing.request_bytes,
            response_bytes=timing.response_bytes,
        ),
    )


def _parse_binary_or_json_response(
    raw: bytes,
    status_code: int,
    url: str,
    transport: str,
    client_timing: ClientTiming | None = None,
) -> InferResult:
    if raw.startswith(_BINARY_RESULT_MAGIC):
        return _parse_binary_infer_response(raw, status_code, url, client_timing)
    return _parse_infer_response(raw, status_code, url, transport, client_timing)


def _parse_infer_response(
    raw: bytes,
    status_code: int,
    url: str,
    transport: str = "json",
    client_timing: ClientTiming | None = None,
) -> InferResult:
    decode_started = time.perf_counter_ns()
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
    return _with_decode_timing(
        InferResult(
            request_id=request_id,
            outputs=outputs,
            correlation_id=correlation_id if isinstance(correlation_id, str) else None,
            timing=_parse_timing(payload.get("timing")),
            transport=transport,
        ),
        client_timing,
        decode_started,
    )


def _parse_binary_infer_response(
    raw: bytes,
    status_code: int,
    url: str,
    client_timing: ClientTiming | None = None,
) -> InferResult:
    del status_code
    decode_started = time.perf_counter_ns()
    metadata, payload = _split_binary_envelope(raw, _BINARY_RESULT_MAGIC, url)
    _require_supported_schema(metadata)
    if metadata.get("status") != "success":
        raise ProtocolError(f"binary serving response from {url!r} did not declare success")
    outputs_obj = metadata.get("outputs")
    if not isinstance(outputs_obj, list) or not outputs_obj:
        raise ProtocolError(f"binary serving success response from {url!r} is missing outputs")
    outputs = tuple(_binary_output_from_metadata(item, payload) for item in outputs_obj)
    request_id = metadata.get("request_id")
    if not isinstance(request_id, str):
        raise ProtocolError(f"binary serving success response from {url!r} is missing request_id")
    correlation_id = metadata.get("correlation_id")
    return _with_decode_timing(
        InferResult(
            request_id=request_id,
            outputs=outputs,
            correlation_id=correlation_id if isinstance(correlation_id, str) else None,
            timing=_parse_timing(metadata.get("timing")),
            transport="binary",
        ),
        client_timing,
        decode_started,
    )


def _split_binary_envelope(raw: bytes, magic: bytes, url: str) -> tuple[dict[str, object], bytes]:
    header_size = len(magic) + 4
    if len(raw) < header_size or not raw.startswith(magic):
        raise ProtocolError(f"binary serving response from {url!r} has invalid magic")
    (metadata_len,) = struct.unpack("<I", raw[len(magic) : header_size])
    metadata_end = header_size + metadata_len
    if len(raw) < metadata_end:
        raise ProtocolError(f"binary serving response from {url!r} has truncated metadata")
    try:
        metadata = json.loads(raw[header_size:metadata_end])
    except ValueError as exc:
        raise ProtocolError(
            f"binary serving response from {url!r} has invalid metadata JSON: {exc}"
        ) from exc
    if not isinstance(metadata, dict):
        raise ProtocolError(f"binary serving response from {url!r} metadata is not an object")
    return metadata, raw[metadata_end:]


def _binary_output_from_metadata(obj: object, payload: bytes) -> TensorOutput:
    if not isinstance(obj, dict):
        raise ProtocolError("binary serving output entry is not a JSON object")
    name = obj.get("name")
    if not isinstance(name, str) or not name:
        raise ProtocolError("binary serving output is missing a non-empty 'name'")
    tensor = obj.get("tensor")
    if not isinstance(tensor, dict):
        raise ProtocolError(f"binary serving output {name!r} is missing tensor metadata")
    dtype_value = tensor.get("dtype")
    try:
        dtype = DType(dtype_value)
    except ValueError as exc:
        raise ProtocolError(
            f"binary serving output {name!r} has unknown dtype {dtype_value!r}"
        ) from exc
    layout_value = tensor.get("layout", Layout.ROW_MAJOR.value)
    try:
        layout = Layout(layout_value)
    except ValueError as exc:
        raise ProtocolError(
            f"binary serving output {name!r} has unknown layout {layout_value!r}"
        ) from exc
    shape_raw = tensor.get("shape")
    if (
        not isinstance(shape_raw, list)
        or not shape_raw
        or not all(
            isinstance(dim, int) and not isinstance(dim, bool) and dim >= 1 for dim in shape_raw
        )
    ):
        raise ProtocolError(f"binary serving output {name!r} has an invalid shape")
    shape = tuple(int(dim) for dim in shape_raw)
    expected_size = itemsize(dtype) * math.prod(shape)
    offset = _required_nonnegative_int(obj, "payload_offset", name)
    size = _required_nonnegative_int(obj, "payload_size", name)
    if size < expected_size:
        raise ProtocolError(
            f"binary serving output {name!r} payload_size is {size}, expected {expected_size}"
        )
    end = offset + size
    if len(payload) < end:
        raise ProtocolError(
            f"binary serving output {name!r} payload is {len(payload)} bytes, need {end}"
        )
    semantic_tag = obj.get("semantic_tag")
    return TensorOutput(
        name=name,
        dtype=dtype,
        shape=shape,
        data=payload[offset : offset + expected_size],
        layout=layout,
        semantic_tag=semantic_tag if isinstance(semantic_tag, str) else None,
    )


def _required_nonnegative_int(obj: dict[str, object], field: str, name: str) -> int:
    value = obj.get(field)
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise ProtocolError(f"binary serving output {name!r} has invalid {field!r}")
    return value


def _is_binary_unsupported(raw: bytes) -> bool:
    try:
        payload = json.loads(raw)
    except (ValueError, UnicodeDecodeError):
        return True
    if not isinstance(payload, dict) or payload.get("status") != "failure":
        return False
    error_obj = payload.get("error")
    return isinstance(error_obj, dict) and error_obj.get("code") == ErrorCode.UNSUPPORTED.value


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

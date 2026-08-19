"""Sidecar IPC protocol constants.

Mirrors the JSON Schema in ``protocol/schemas/python_pytorch_ipc.json``.
Keep the literals here in sync; the schema is the source of truth.
"""

from __future__ import annotations

from typing import Final

#: The JSON header always carries this field with this value.
SCHEMA_VERSION: Final[str] = "0.1"

# Request kinds
KIND_LOAD_MODEL: Final[str] = "load_model"
KIND_PRIME: Final[str] = "prime"
KIND_INFER: Final[str] = "infer"
KIND_INFER_ASYNC: Final[str] = "infer_async"
KIND_CANCEL: Final[str] = "cancel"
KIND_UNLOAD: Final[str] = "unload"
KIND_HEALTH_CHECK: Final[str] = "health_check"

# Response kinds (each request kind has a matching response kind)
KIND_LOAD_MODEL_RESPONSE: Final[str] = "load_model_response"
KIND_PRIME_RESPONSE: Final[str] = "prime_response"
KIND_INFER_RESPONSE: Final[str] = "infer_response"
KIND_INFER_ASYNC_RESPONSE: Final[str] = "infer_async_response"
KIND_CANCEL_RESPONSE: Final[str] = "cancel_response"
KIND_UNLOAD_RESPONSE: Final[str] = "unload_response"
KIND_HEALTH_CHECK_RESPONSE: Final[str] = "health_check_response"

# Event kinds (unsolicited)
KIND_READY_EVENT: Final[str] = "ready_event"
KIND_ERROR_EVENT: Final[str] = "error_event"
KIND_METRIC_EVENT: Final[str] = "metric_event"

REQUEST_TO_RESPONSE: Final[dict[str, str]] = {
    KIND_LOAD_MODEL: KIND_LOAD_MODEL_RESPONSE,
    KIND_PRIME: KIND_PRIME_RESPONSE,
    KIND_INFER: KIND_INFER_RESPONSE,
    KIND_INFER_ASYNC: KIND_INFER_ASYNC_RESPONSE,
    KIND_CANCEL: KIND_CANCEL_RESPONSE,
    KIND_UNLOAD: KIND_UNLOAD_RESPONSE,
    KIND_HEALTH_CHECK: KIND_HEALTH_CHECK_RESPONSE,
}

# Status discriminator on response and error-event messages.
STATUS_OK: Final[str] = "ok"
STATUS_ERROR: Final[str] = "error"

# Error codes mirror tensorplate::Error::Code snake_case wire names.
ERR_CONFIG_INVALID: Final[str] = "config_invalid"
ERR_LOAD_FAILED: Final[str] = "load_failed"
ERR_NOT_READY: Final[str] = "not_ready"
ERR_SHAPE_MISMATCH: Final[str] = "shape_mismatch"
ERR_UNSUPPORTED: Final[str] = "unsupported"
ERR_OOM_ERROR: Final[str] = "oom_error"
ERR_TIMEOUT: Final[str] = "timeout"
ERR_INFERENCE_FAILED: Final[str] = "inference_failed"
ERR_INTERNAL: Final[str] = "internal"

# Vendor-neutral platform reason carried by a runtime capability record.
REASON_ACCELERATOR_RUNTIME_UNAVAILABLE: Final[str] = "accelerator_runtime_unavailable"


__all__ = [
    "ERR_CONFIG_INVALID",
    "ERR_INFERENCE_FAILED",
    "ERR_INTERNAL",
    "ERR_LOAD_FAILED",
    "ERR_NOT_READY",
    "ERR_OOM_ERROR",
    "ERR_SHAPE_MISMATCH",
    "ERR_TIMEOUT",
    "ERR_UNSUPPORTED",
    "KIND_CANCEL",
    "KIND_CANCEL_RESPONSE",
    "KIND_ERROR_EVENT",
    "KIND_HEALTH_CHECK",
    "KIND_HEALTH_CHECK_RESPONSE",
    "KIND_INFER",
    "KIND_INFER_ASYNC",
    "KIND_INFER_ASYNC_RESPONSE",
    "KIND_INFER_RESPONSE",
    "KIND_LOAD_MODEL",
    "KIND_LOAD_MODEL_RESPONSE",
    "KIND_METRIC_EVENT",
    "KIND_PRIME",
    "KIND_PRIME_RESPONSE",
    "KIND_READY_EVENT",
    "KIND_UNLOAD",
    "KIND_UNLOAD_RESPONSE",
    "REASON_ACCELERATOR_RUNTIME_UNAVAILABLE",
    "REQUEST_TO_RESPONSE",
    "SCHEMA_VERSION",
    "STATUS_ERROR",
    "STATUS_OK",
]

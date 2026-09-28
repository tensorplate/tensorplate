"""Candidate-only text and JSON tensors for the speech runner profiles.

On the tensor-only ``infer`` path a speech runner takes text as a
one-dimensional ``uint8`` tensor named ``text_utf8`` and returns its
structured result as one named ``result_json``, each at most 1 MiB. The
convention lasts only until the speech job messages replace it, so only
runner profiles (modules under ``backends``) import this module.
"""

from __future__ import annotations

import json
from collections.abc import Mapping, Sequence
from typing import Any, Final, NoReturn

from tensorplate_pytorch_backend.backends.base import BackendError, NamedTensor
from tensorplate_pytorch_backend.protocol import (
    ERR_CONFIG_INVALID,
    ERR_INFERENCE_FAILED,
    ERR_SHAPE_MISMATCH,
)

TEXT_UTF8: Final[str] = "text_utf8"
RESULT_JSON: Final[str] = "result_json"
MAX_TENSOR_BYTES: Final[int] = 1 << 20


def _bytes_tensor(name: str, payload: bytes) -> NamedTensor:
    return NamedTensor(
        name=name, tensor={"dtype": "uint8", "shape": [len(payload)]}, payload=payload
    )


def _only_bytes(tensors: Sequence[NamedTensor], name: str) -> bytes:
    found = [tensor for tensor in tensors if tensor.name == name]
    if len(found) != 1:
        raise BackendError(ERR_SHAPE_MISMATCH, f"expected exactly one `{name}` tensor")
    tensor = found[0]
    shape = tensor.tensor.get("shape")
    if (
        tensor.tensor.get("dtype") != "uint8"
        or not isinstance(shape, list)
        or len(shape) != 1
        or type(shape[0]) is not int
        or shape[0] != len(tensor.payload)
    ):
        raise BackendError(
            ERR_SHAPE_MISMATCH, f"`{name}` must be a one-dimensional uint8 tensor of its payload"
        )
    if len(tensor.payload) > MAX_TENSOR_BYTES:
        raise BackendError(ERR_CONFIG_INVALID, f"`{name}` exceeds {MAX_TENSOR_BYTES} bytes")
    return tensor.payload


def _utf8(payload: bytes, name: str) -> str:
    try:
        return payload.decode("utf-8")
    except UnicodeDecodeError:
        raise BackendError(ERR_CONFIG_INVALID, f"`{name}` is not valid UTF-8") from None


def text_utf8_tensor(text: str) -> NamedTensor:
    """Encode ``text`` as the ``text_utf8`` input tensor."""
    return _bytes_tensor(TEXT_UTF8, text.encode("utf-8"))


def read_text_utf8(inputs: Sequence[NamedTensor]) -> str:
    """Return the non-empty text of the one ``text_utf8`` input."""
    text = _utf8(_only_bytes(inputs, TEXT_UTF8), TEXT_UTF8)
    if not text:
        raise BackendError(ERR_CONFIG_INVALID, f"`{TEXT_UTF8}` is empty")
    return text


def result_json_tensor(result: Mapping[str, Any]) -> NamedTensor:
    """Encode a runner result as the ``result_json`` output tensor."""
    try:
        payload = json.dumps(
            dict(result), ensure_ascii=False, allow_nan=False, separators=(",", ":"), sort_keys=True
        ).encode("utf-8")
    except (TypeError, ValueError):
        raise BackendError(ERR_INFERENCE_FAILED, "the runner result is not JSON") from None
    if len(payload) > MAX_TENSOR_BYTES:
        raise BackendError(
            ERR_INFERENCE_FAILED, f"the runner result exceeds {MAX_TENSOR_BYTES} bytes"
        )
    return _bytes_tensor(RESULT_JSON, payload)


def _no_constant(token: str) -> NoReturn:
    raise ValueError(token)


def read_result_json(outputs: Sequence[NamedTensor]) -> dict[str, Any]:
    """Decode the JSON object of the one ``result_json`` output."""
    text = _utf8(_only_bytes(outputs, RESULT_JSON), RESULT_JSON)
    try:
        result = json.loads(text, parse_constant=_no_constant)
    except ValueError:
        raise BackendError(ERR_CONFIG_INVALID, f"`{RESULT_JSON}` is not JSON") from None
    if not isinstance(result, dict):
        raise BackendError(ERR_CONFIG_INVALID, f"`{RESULT_JSON}` must be a JSON object")
    return result


__all__ = [
    "MAX_TENSOR_BYTES",
    "RESULT_JSON",
    "TEXT_UTF8",
    "read_result_json",
    "read_text_utf8",
    "result_json_tensor",
    "text_utf8_tensor",
]

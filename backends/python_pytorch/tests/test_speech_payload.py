"""The candidate-only text_utf8 / result_json tensors."""

from __future__ import annotations

import ast
import json
import math
from pathlib import Path
from typing import Any

import pytest

from tensorplate_pytorch_backend import protocol, speech_payload
from tensorplate_pytorch_backend.backends.base import BackendError, NamedTensor
from tensorplate_pytorch_backend.speech_payload import (
    MAX_TENSOR_BYTES,
    RESULT_JSON,
    TEXT_UTF8,
    read_result_json,
    read_text_utf8,
    result_json_tensor,
    text_utf8_tensor,
)

_REPO = Path(__file__).resolve().parents[3]
_SEAM = json.loads((_REPO / "protocol" / "fixtures" / "job_seam.json").read_text(encoding="utf-8"))
_PACKAGE = Path(speech_payload.__file__).resolve().parent
_CANARY = "tp-canary-7f3a9c"


def _raw(name: str, payload: bytes, **tensor: Any) -> NamedTensor:
    meta: dict[str, Any] = {"dtype": "uint8", "shape": [len(payload)]}
    meta.update(tensor)
    return NamedTensor(name=name, tensor=meta, payload=payload)


def _refused(call: Any, *args: Any) -> BackendError:
    with pytest.raises(BackendError) as caught:
        call(*args)
    return caught.value


def _seam_texts(reason: str) -> list[bytes]:
    texts = []
    for vector in _SEAM["requests"]:
        payload = vector["request"]["payload"]
        if payload.get("type") == "text_segment" and vector["expect"] == {"reject": reason}:
            texts.append(bytes.fromhex(payload["text"]["hex"]))
    return texts


@pytest.mark.parametrize("text", ["a", "héllo wörld", "مرحبا", "😀" * 3, "\0 is a code point"])
def test_text_round_trips_through_the_tensor(text: str) -> None:
    tensor = text_utf8_tensor(text)
    assert tensor.name == TEXT_UTF8
    assert tensor.tensor == {"dtype": "uint8", "shape": [len(text.encode("utf-8"))]}
    assert read_text_utf8([_raw("other", b"x"), tensor]) == text


def test_the_job_seam_invalid_utf8_vectors_are_refused() -> None:
    invalid = _seam_texts("text_invalid_utf8")
    assert len(invalid) >= 6
    for payload in invalid:
        err = _refused(read_text_utf8, [_raw(TEXT_UTF8, payload)])
        assert err.code == protocol.ERR_CONFIG_INVALID
        assert err.code_message == "`text_utf8` is not valid UTF-8"


def test_refusing_text_never_echoes_it() -> None:
    err = _refused(read_text_utf8, [_raw(TEXT_UTF8, _CANARY.encode() + b"\xff")])
    assert _CANARY not in err.code_message


def test_empty_text_is_refused() -> None:
    err = _refused(read_text_utf8, [_raw(TEXT_UTF8, b"")])
    assert (err.code, err.code_message) == (protocol.ERR_CONFIG_INVALID, "`text_utf8` is empty")


def test_text_up_to_one_mebibyte_is_read_and_beyond_is_refused() -> None:
    assert len(read_text_utf8([_raw(TEXT_UTF8, b"a" * MAX_TENSOR_BYTES)])) == MAX_TENSOR_BYTES
    err = _refused(read_text_utf8, [_raw(TEXT_UTF8, b"a" * (MAX_TENSOR_BYTES + 1))])
    assert err.code == protocol.ERR_CONFIG_INVALID


@pytest.mark.parametrize(
    "inputs",
    [
        [],
        [_raw("text", b"a")],
        [_raw(TEXT_UTF8, b"a"), _raw(TEXT_UTF8, b"b")],
        [_raw(TEXT_UTF8, b"a", dtype="int8")],
        [_raw(TEXT_UTF8, b"ab", shape=[1, 2])],
        [_raw(TEXT_UTF8, b"ab", shape=[3])],
        [_raw(TEXT_UTF8, b"a", shape=[True])],
        [_raw(TEXT_UTF8, b"a", shape=1)],
    ],
)
def test_a_malformed_tensor_is_a_shape_mismatch(inputs: list[NamedTensor]) -> None:
    assert _refused(read_text_utf8, inputs).code == protocol.ERR_SHAPE_MISMATCH


def test_results_are_compact_sorted_utf8_json() -> None:
    tensor = result_json_tensor({"text": "مرحبا", "segments": [], "a": 1})
    assert tensor.name == RESULT_JSON
    assert tensor.payload == '{"a":1,"segments":[],"text":"مرحبا"}'.encode()
    assert tensor.tensor == {"dtype": "uint8", "shape": [len(tensor.payload)]}
    assert read_result_json([tensor]) == {"a": 1, "segments": [], "text": "مرحبا"}


@pytest.mark.parametrize(
    "result", [{"x": math.nan}, {"x": math.inf}, {"x": {1, 2}}, {"x": b"a"}, {"x": "a\ud800"}]
)
def test_a_result_that_is_not_json_fails_inference(result: dict[str, Any]) -> None:
    err = _refused(result_json_tensor, result)
    assert err.code == protocol.ERR_INFERENCE_FAILED
    assert err.code_message == "the runner result is not JSON"


def test_a_result_up_to_one_mebibyte_is_encoded_and_beyond_fails() -> None:
    at_bound = {"t": "a" * (MAX_TENSOR_BYTES - len('{"t":""}'))}
    assert len(result_json_tensor(at_bound).payload) == MAX_TENSOR_BYTES
    err = _refused(result_json_tensor, {"t": at_bound["t"] + "a"})
    assert err.code == protocol.ERR_INFERENCE_FAILED


@pytest.mark.parametrize(
    "payload",
    [b'{"x":NaN}', b'{"x":-Infinity}', b"[1]", b'"text"', b"{", b'{"x":"\xff"}'],
)
def test_reading_a_result_that_is_not_a_json_object_is_refused(payload: bytes) -> None:
    assert _refused(read_result_json, [_raw(RESULT_JSON, payload)]).code == (
        protocol.ERR_CONFIG_INVALID
    )


def _modules_importing(module: str) -> set[str]:
    importers = set()
    for path in _PACKAGE.rglob("*.py"):
        tree = ast.parse(path.read_text(encoding="utf-8"))
        for node in ast.walk(tree):
            names: list[str] = []
            if isinstance(node, ast.Import):
                names = [alias.name for alias in node.names]
            elif isinstance(node, ast.ImportFrom):
                base = node.module or ""
                names = [base, *(f"{base}.{alias.name}" for alias in node.names)]
            if any(name.endswith(module) for name in names):
                importers.add(path.relative_to(_PACKAGE).as_posix())
    return importers


def test_only_runner_profiles_import_the_candidate_convention() -> None:
    # Control: the scan does see a top-level module's import.
    assert "runner.py" in _modules_importing("configuration")
    importers = _modules_importing("speech_payload")
    assert all(path.startswith("backends/") for path in importers), importers

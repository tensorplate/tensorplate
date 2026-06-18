"""Unit tests for tensor value objects and v0.1 envelope marshalling."""

from __future__ import annotations

import base64
import struct

import pytest

from tensorplate.errors import ProtocolError
from tensorplate.tensors import DType, Layout, TensorInput, TensorOutput, itemsize


def test_named_input_marshalling() -> None:
    data = struct.pack("<4f", 1.0, 2.0, 3.0, 4.0)
    named = TensorInput("x", DType.FLOAT32, (1, 4), data).to_named_input()
    assert named["name"] == "x"
    tensor = named["tensor"]
    assert isinstance(tensor, dict)
    assert tensor["dtype"] == "float32"
    assert tensor["layout"] == "row_major"
    assert tensor["shape"] == [1, 4]
    assert tensor["byte_size"] == 16
    payload = named["payload_b64"]
    assert isinstance(payload, str)
    assert base64.b64decode(payload) == data


def test_input_rejects_data_size_mismatch() -> None:
    with pytest.raises(ValueError, match="expected"):
        TensorInput("x", DType.FLOAT32, (2, 2), b"\x00")


def test_itemsize_covers_every_dtype() -> None:
    expected = {
        "float32": 4,
        "float16": 2,
        "bfloat16": 2,
        "int64": 8,
        "int32": 4,
        "int16": 2,
        "int8": 1,
        "uint8": 1,
        "bool": 1,
    }
    assert {dtype.value: itemsize(dtype) for dtype in DType} == expected


def test_output_round_trips_through_named_output() -> None:
    data = struct.pack("<2i", 7, 9)
    named = TensorInput("o", DType.INT32, (2,), data).to_named_input()
    out = TensorOutput.from_named_output(named)
    assert out.name == "o"
    assert out.dtype is DType.INT32
    assert out.shape == (2,)
    assert out.layout is Layout.ROW_MAJOR
    assert out.data == data


def test_output_honors_byte_offset_and_size() -> None:
    payload = b"\xaa\xaa" + struct.pack("<i", 5)
    obj = {
        "name": "o",
        "tensor": {"dtype": "int32", "shape": [1], "byte_offset": 2, "byte_size": 4},
        "payload_b64": base64.b64encode(payload).decode("ascii"),
    }
    assert TensorOutput.from_named_output(obj).data == struct.pack("<i", 5)


def test_output_accepts_zero_and_padded_byte_size() -> None:
    data = struct.pack("<i", 5)
    for byte_size in (0, 8):
        obj = {
            "name": "o",
            "tensor": {"dtype": "int32", "shape": [1], "byte_size": byte_size},
            "payload_b64": base64.b64encode(data + b"\xff" * 4).decode("ascii"),
        }
        assert TensorOutput.from_named_output(obj).data == data


def test_output_reports_semantic_tag() -> None:
    obj = {
        "name": "d",
        "tensor": {"dtype": "uint8", "shape": [1]},
        "payload_b64": base64.b64encode(b"\x01").decode("ascii"),
        "semantic_tag": "detections",
    }
    assert TensorOutput.from_named_output(obj).semantic_tag == "detections"


def test_output_rejects_unknown_dtype() -> None:
    obj = {
        "name": "o",
        "tensor": {"dtype": "complex128", "shape": [1]},
        "payload_b64": base64.b64encode(b"\x00" * 16).decode("ascii"),
    }
    with pytest.raises(ProtocolError):
        TensorOutput.from_named_output(obj)


@pytest.mark.parametrize(
    ("tensor", "match"),
    [
        ({"dtype": "int32", "shape": [0], "byte_size": 4}, "shape"),
        ({"dtype": "int32", "shape": [1], "byte_offset": -1, "byte_size": 4}, "byte_offset"),
        ({"dtype": "int32", "shape": [1], "byte_size": -4}, "byte_size"),
        ({"dtype": "int32", "shape": [1], "byte_size": 2}, "expected 4"),
    ],
)
def test_output_rejects_invalid_tensor_metadata(tensor: dict[str, object], match: str) -> None:
    obj = {
        "name": "o",
        "tensor": tensor,
        "payload_b64": base64.b64encode(b"\x00\x00\x00\x00").decode("ascii"),
    }
    with pytest.raises(ProtocolError, match=match):
        TensorOutput.from_named_output(obj)


def test_output_rejects_short_payload() -> None:
    obj = {
        "name": "o",
        "tensor": {"dtype": "int32", "shape": [4], "byte_size": 16},
        "payload_b64": base64.b64encode(b"\x00\x00\x00\x00").decode("ascii"),
    }
    with pytest.raises(ProtocolError):
        TensorOutput.from_named_output(obj)


def test_numpy_round_trip() -> None:
    pytest.importorskip("numpy")
    import numpy as np

    array = np.arange(6, dtype=np.float32).reshape(2, 3)
    built = TensorInput.from_numpy("x", array)
    assert built.dtype is DType.FLOAT32
    assert built.shape == (2, 3)
    restored = TensorOutput("x", DType.FLOAT32, (2, 3), built.data).to_numpy()
    assert np.array_equal(restored, array)


def test_bfloat16_array_access_raises() -> None:
    out = TensorOutput("x", DType.BFLOAT16, (2,), b"\x00\x00\x00\x00")
    with pytest.raises(ValueError, match="numpy mapping"):
        out.to_numpy()

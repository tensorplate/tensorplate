"""Tensor value objects and v0.1 serving-envelope tensor marshalling.

Tensors travel over the wire as base64-encoded raw bytes plus a small
metadata block (``dtype``, ``shape``, ``layout``, ``byte_offset``,
``byte_size``). These value objects own that marshalling so the client
and the vision helpers share one representation. ``numpy`` is an optional
dependency: array access imports it lazily and fails with a clear message
when it is absent.
"""

from __future__ import annotations

import base64
import math
from dataclasses import dataclass
from enum import Enum
from typing import TYPE_CHECKING, Literal

from tensorplate.errors import ProtocolError

if TYPE_CHECKING:
    import numpy as np


class DType(str, Enum):
    """Supported v0.1 envelope tensor dtypes."""

    FLOAT32 = "float32"
    FLOAT16 = "float16"
    BFLOAT16 = "bfloat16"
    INT64 = "int64"
    INT32 = "int32"
    INT16 = "int16"
    INT8 = "int8"
    UINT8 = "uint8"
    BOOL = "bool"


class Layout(str, Enum):
    """Memory layout of the raw tensor bytes."""

    ROW_MAJOR = "row_major"
    COL_MAJOR = "col_major"


#: Bytes per element for each dtype.
_ITEMSIZE: dict[DType, int] = {
    DType.FLOAT32: 4,
    DType.FLOAT16: 2,
    DType.BFLOAT16: 2,
    DType.INT64: 8,
    DType.INT32: 4,
    DType.INT16: 2,
    DType.INT8: 1,
    DType.UINT8: 1,
    DType.BOOL: 1,
}

#: dtypes with a native numpy mapping. ``bfloat16`` is intentionally
#: absent: numpy has no native bfloat16, so array access for it raises.
_NUMPY_NAME: dict[DType, str] = {
    DType.FLOAT32: "float32",
    DType.FLOAT16: "float16",
    DType.INT64: "int64",
    DType.INT32: "int32",
    DType.INT16: "int16",
    DType.INT8: "int8",
    DType.UINT8: "uint8",
    DType.BOOL: "bool",
}

_NUMPY_NAME_TO_DTYPE: dict[str, DType] = {name: dt for dt, name in _NUMPY_NAME.items()}


def itemsize(dtype: DType) -> int:
    """Return the number of bytes per element for ``dtype``."""
    return _ITEMSIZE[dtype]


@dataclass(frozen=True)
class TensorInput:
    """A single named input tensor ready to marshal into the v0.1 envelope."""

    name: str
    dtype: DType
    shape: tuple[int, ...]
    data: bytes
    layout: Layout = Layout.ROW_MAJOR

    def __post_init__(self) -> None:
        if not self.name:
            raise ValueError("tensor input name must be non-empty")
        if not self.shape:
            raise ValueError(f"tensor input {self.name!r} must have at least one dimension")
        if any(dim < 1 for dim in self.shape):
            raise ValueError(
                f"tensor input {self.name!r} has a non-positive dimension: {self.shape}"
            )
        expected = _ITEMSIZE[self.dtype] * math.prod(self.shape)
        if len(self.data) != expected:
            raise ValueError(
                f"tensor input {self.name!r}: data is {len(self.data)} bytes, expected {expected} "
                f"for dtype {self.dtype.value} shape {self.shape}"
            )

    def to_named_input(self) -> dict[str, object]:
        """Serialize to a v0.1 ``NamedInput`` envelope object."""
        return {
            "name": self.name,
            "tensor": {
                "dtype": self.dtype.value,
                "layout": self.layout.value,
                "shape": list(self.shape),
                "byte_offset": 0,
                "byte_size": len(self.data),
            },
            "payload_b64": base64.b64encode(self.data).decode("ascii"),
        }

    @classmethod
    def from_numpy(
        cls,
        name: str,
        array: np.ndarray,
        *,
        dtype: DType | None = None,
        layout: Layout = Layout.ROW_MAJOR,
    ) -> TensorInput:
        """Build a ``TensorInput`` from a numpy array (numpy required)."""
        resolved = dtype if dtype is not None else _NUMPY_NAME_TO_DTYPE.get(str(array.dtype))
        if resolved is None:
            raise ValueError(
                f"numpy dtype {array.dtype!r} has no v0.1 envelope mapping; "
                "pass `dtype=` explicitly"
            )
        order: Literal["C", "F"] = "C" if layout is Layout.ROW_MAJOR else "F"
        data = array.astype(_NUMPY_NAME[resolved], copy=False).tobytes(order=order)
        return cls(
            name=name,
            dtype=resolved,
            shape=tuple(int(dim) for dim in array.shape),
            data=data,
            layout=layout,
        )


@dataclass(frozen=True)
class TensorOutput:
    """A single named output tensor parsed from a v0.1 success envelope."""

    name: str
    dtype: DType
    shape: tuple[int, ...]
    data: bytes
    layout: Layout = Layout.ROW_MAJOR
    semantic_tag: str | None = None

    @classmethod
    def from_named_output(cls, obj: object) -> TensorOutput:
        """Parse a v0.1 ``NamedOutput`` envelope object."""
        if not isinstance(obj, dict):
            raise ProtocolError("serving output entry is not a JSON object")
        name = obj.get("name")
        if not isinstance(name, str) or not name:
            raise ProtocolError("serving output is missing a non-empty 'name'")
        tensor = obj.get("tensor")
        if not isinstance(tensor, dict):
            raise ProtocolError(f"serving output {name!r} is missing its 'tensor' metadata")
        dtype_value = tensor.get("dtype")
        try:
            dtype = DType(dtype_value)
        except ValueError as exc:
            raise ProtocolError(
                f"serving output {name!r} has unknown dtype {dtype_value!r}"
            ) from exc
        layout_value = tensor.get("layout", Layout.ROW_MAJOR.value)
        try:
            layout = Layout(layout_value)
        except ValueError as exc:
            raise ProtocolError(
                f"serving output {name!r} has unknown layout {layout_value!r}"
            ) from exc
        shape_raw = tensor.get("shape")
        if not isinstance(shape_raw, list) or not all(isinstance(dim, int) for dim in shape_raw):
            raise ProtocolError(f"serving output {name!r} has an invalid 'shape'")
        payload_b64 = obj.get("payload_b64")
        if not isinstance(payload_b64, str):
            raise ProtocolError(f"serving output {name!r} is missing 'payload_b64'")
        try:
            raw = base64.b64decode(payload_b64, validate=True)
        except ValueError as exc:
            raise ProtocolError(f"serving output {name!r} has an invalid base64 payload") from exc
        offset = tensor.get("byte_offset", 0)
        offset = offset if isinstance(offset, int) else 0
        size = tensor.get("byte_size")
        if isinstance(size, int):
            end = offset + size
            if len(raw) < end:
                raise ProtocolError(
                    f"serving output {name!r} payload is {len(raw)} bytes, need {end}"
                )
            data = raw[offset:end]
        else:
            data = raw[offset:]
        semantic_tag = obj.get("semantic_tag")
        return cls(
            name=name,
            dtype=dtype,
            shape=tuple(int(dim) for dim in shape_raw),
            data=data,
            layout=layout,
            semantic_tag=semantic_tag if isinstance(semantic_tag, str) else None,
        )

    def to_numpy(self) -> np.ndarray:
        """Return the tensor as a numpy array (numpy required).

        Raises ``ValueError`` for dtypes with no native numpy mapping
        (``bfloat16``); use :attr:`data` for the raw bytes in that case.
        """
        numpy_name = _NUMPY_NAME.get(self.dtype)
        if numpy_name is None:
            raise ValueError(
                f"dtype {self.dtype.value!r} has no native numpy mapping; read `.data` instead"
            )
        try:
            import numpy
        except ModuleNotFoundError as exc:  # pragma: no cover - exercised only without numpy
            raise RuntimeError(
                "tensor array access requires numpy; install `tensorplate-python[numpy]`"
            ) from exc
        array = numpy.frombuffer(self.data, dtype=numpy_name)
        order: Literal["C", "F"] = "C" if self.layout is Layout.ROW_MAJOR else "F"
        return array.reshape(self.shape, order=order)

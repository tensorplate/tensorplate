"""TensorPlate Python SDK.

Client-side SDK for calling already-deployed TensorPlate detection and
vision serving models over the v0.1 ``/infer`` HTTP envelope. The public
surface is re-exported here; see the individual module docstrings for
details.
"""

from __future__ import annotations

from importlib.metadata import PackageNotFoundError, version

from tensorplate.client import (
    LOOPBACK_DEFAULT,
    ResolvedEndpoint,
    canonicalize_serving_url,
    resolve_serving_url,
)
from tensorplate.errors import (
    EndpointResolutionError,
    ErrorCode,
    ProtocolError,
    RequestTimeoutError,
    ServingError,
    TensorPlateError,
    TransportError,
    UnsupportedSchemaVersionError,
)
from tensorplate.postprocess import Detection
from tensorplate.serving import HealthSnapshot, InferResult, ServingClient, Timing
from tensorplate.tensors import DType, Layout, TensorInput, TensorOutput
from tensorplate.vision import VisionClient

try:
    __version__ = version("tensorplate-python")
except PackageNotFoundError:  # pragma: no cover - only hit outside an installed package
    __version__ = "0.0.0+unknown"

__all__ = [
    "LOOPBACK_DEFAULT",
    "DType",
    "Detection",
    "EndpointResolutionError",
    "ErrorCode",
    "HealthSnapshot",
    "InferResult",
    "Layout",
    "ProtocolError",
    "RequestTimeoutError",
    "ResolvedEndpoint",
    "ServingClient",
    "ServingError",
    "TensorInput",
    "TensorOutput",
    "TensorPlateError",
    "Timing",
    "TransportError",
    "UnsupportedSchemaVersionError",
    "VisionClient",
    "__version__",
    "canonicalize_serving_url",
    "resolve_serving_url",
]

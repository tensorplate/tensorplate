"""TensorPlate Python SDK.

Client-side SDK for calling already-deployed TensorPlate detection and
vision serving models over the v0.1 ``/infer`` HTTP envelope. The public
surface is re-exported here; see the individual module docstrings for
details.
"""

from __future__ import annotations

from importlib.metadata import PackageNotFoundError, version

from tensorplate.errors import TensorPlateError
from tensorplate.postprocess import Detection
from tensorplate.serving import ServingClient
from tensorplate.vision import VisionClient

try:
    __version__ = version("tensorplate-python")
except PackageNotFoundError:  # pragma: no cover - only hit outside an installed package
    __version__ = "0.0.0+unknown"

__all__ = [
    "Detection",
    "ServingClient",
    "TensorPlateError",
    "VisionClient",
    "__version__",
]

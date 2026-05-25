"""Per-bundle backend implementations selectable from the sidecar runner."""

from tensorplate_pytorch_backend.backends.base import (
    Backend,
    BackendError,
    NamedTensor,
)
from tensorplate_pytorch_backend.backends.fixture import FixtureBackend
from tensorplate_pytorch_backend.backends.smolvla import SmolVLABackend

__all__ = [
    "Backend",
    "BackendError",
    "FixtureBackend",
    "NamedTensor",
    "SmolVLABackend",
]

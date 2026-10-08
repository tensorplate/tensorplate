"""Per-bundle backend implementations selectable from the sidecar runner."""

from tensorplate_pytorch_backend.backends.base import (
    Backend,
    BackendError,
    JobBackend,
    NamedTensor,
    RuntimeCapability,
)
from tensorplate_pytorch_backend.backends.cuda_fixture import CudaFixtureBackend
from tensorplate_pytorch_backend.backends.faster_whisper import FasterWhisperBackend
from tensorplate_pytorch_backend.backends.fixture import FixtureBackend
from tensorplate_pytorch_backend.backends.kokoro import KokoroBackend
from tensorplate_pytorch_backend.backends.mps_fixture import MpsFixtureBackend
from tensorplate_pytorch_backend.backends.smolvla import SmolVLABackend

__all__ = [
    "Backend",
    "BackendError",
    "CudaFixtureBackend",
    "FasterWhisperBackend",
    "FixtureBackend",
    "JobBackend",
    "KokoroBackend",
    "MpsFixtureBackend",
    "NamedTensor",
    "RuntimeCapability",
    "SmolVLABackend",
]

"""Sidecar-private accelerator runtime probing."""

from __future__ import annotations

import platform
from typing import Protocol

from tensorplate_pytorch_backend.backends.base import BackendError, RuntimeCapability
from tensorplate_pytorch_backend.protocol import (
    ERR_UNSUPPORTED,
    REASON_ACCELERATOR_RUNTIME_UNAVAILABLE,
)


class _MpsApi(Protocol):
    def is_built(self) -> bool: ...

    def is_available(self) -> bool: ...


class _TorchBackendsApi(Protocol):
    mps: _MpsApi


class _TorchApi(Protocol):
    __version__: object
    backends: _TorchBackendsApi


class _CudaApi(Protocol):
    def is_available(self) -> bool: ...


class _TorchVersionApi(Protocol):
    cuda: str | None


class _TorchCudaApi(Protocol):
    __version__: object
    cuda: _CudaApi
    version: _TorchVersionApi


def probe_mps_runtime(
    torch_module: _TorchApi, *, accelerator_runtime_version: str | None = None
) -> RuntimeCapability:
    """Collect normalized facts from PyTorch's MPS runtime boundary."""

    framework_version = str(getattr(torch_module, "__version__", "unknown")) or "unknown"
    runtime_version = (
        accelerator_runtime_version or platform.mac_ver()[0] or platform.release() or "unknown"
    )

    try:
        mps = torch_module.backends.mps
        runtime_built = bool(mps.is_built())
        runtime_available = runtime_built and bool(mps.is_available())
    except Exception:
        runtime_built = False
        runtime_available = False

    return RuntimeCapability(
        backend_name="python_pytorch",
        framework_version=framework_version,
        accelerator_runtime_version=runtime_version,
        accelerator_runtime_built=runtime_built,
        accelerator_runtime_available=runtime_available,
        unavailable_reason=(None if runtime_available else REASON_ACCELERATOR_RUNTIME_UNAVAILABLE),
    )


def require_mps_runtime(
    torch_module: _TorchApi, *, accelerator_runtime_version: str | None = None
) -> RuntimeCapability:
    """Return the MPS capability or reject before model loading."""

    capability = probe_mps_runtime(
        torch_module, accelerator_runtime_version=accelerator_runtime_version
    )
    if not capability.accelerator_runtime_available:
        raise BackendError(
            ERR_UNSUPPORTED,
            f"{REASON_ACCELERATOR_RUNTIME_UNAVAILABLE}: "
            "the configured accelerator runtime is not available",
            context=f"{REASON_ACCELERATOR_RUNTIME_UNAVAILABLE}; "
            f"built={str(capability.accelerator_runtime_built).lower()}",
            runtime_capability=capability,
        )
    return capability


def probe_cuda_runtime(
    torch_module: _TorchCudaApi, *, accelerator_runtime_version: str | None = None
) -> RuntimeCapability:
    """Collect normalized facts from PyTorch's CUDA runtime boundary."""

    framework_version = str(getattr(torch_module, "__version__", "unknown")) or "unknown"

    # PyTorch exposes no `is_built()` for CUDA the way it does for MPS;
    # `torch.version.cuda` is `None` in a CPU-only build and the toolkit
    # version otherwise, so it carries the same meaning here.
    try:
        built_version = getattr(torch_module.version, "cuda", None)
    except Exception:
        built_version = None
    runtime_built = built_version is not None

    try:
        runtime_available = runtime_built and bool(torch_module.cuda.is_available())
    except Exception:
        runtime_available = False

    runtime_version = accelerator_runtime_version or (
        str(built_version) if built_version else "unknown"
    )

    return RuntimeCapability(
        backend_name="python_pytorch",
        framework_version=framework_version,
        accelerator_runtime_version=runtime_version,
        accelerator_runtime_built=runtime_built,
        accelerator_runtime_available=runtime_available,
        unavailable_reason=(None if runtime_available else REASON_ACCELERATOR_RUNTIME_UNAVAILABLE),
    )


def require_cuda_runtime(
    torch_module: _TorchCudaApi, *, accelerator_runtime_version: str | None = None
) -> RuntimeCapability:
    """Return the CUDA capability or reject before model loading."""

    capability = probe_cuda_runtime(
        torch_module, accelerator_runtime_version=accelerator_runtime_version
    )
    if not capability.accelerator_runtime_available:
        raise BackendError(
            ERR_UNSUPPORTED,
            f"{REASON_ACCELERATOR_RUNTIME_UNAVAILABLE}: "
            "the configured accelerator runtime is not available",
            context=f"{REASON_ACCELERATOR_RUNTIME_UNAVAILABLE}; "
            f"built={str(capability.accelerator_runtime_built).lower()}",
            runtime_capability=capability,
        )
    return capability


__all__ = [
    "probe_cuda_runtime",
    "probe_mps_runtime",
    "require_cuda_runtime",
    "require_mps_runtime",
]

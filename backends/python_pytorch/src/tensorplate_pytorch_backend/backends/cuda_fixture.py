"""CUDA-backed fixture that proves a packaged deploy reaches the accelerator."""

from __future__ import annotations

from typing import Any

from tensorplate_pytorch_backend.accelerator import require_cuda_runtime
from tensorplate_pytorch_backend.backends.base import BackendError, RuntimeCapability
from tensorplate_pytorch_backend.backends.fixture import FixtureBackend
from tensorplate_pytorch_backend.configuration import ArtifactConfigError, read_artifact_config
from tensorplate_pytorch_backend.protocol import ERR_CONFIG_INVALID, ERR_LOAD_FAILED

# A matmul rather than an elementwise op: the speech runners reach the GPU
# through cuBLAS, so this is the library path worth proving from inside the
# service sandbox.
_PROBE_EXTENT = 64


class CudaFixtureBackend(FixtureBackend):
    """Echo fixture whose load succeeds only after a CUDA matmul."""

    def __init__(self) -> None:
        super().__init__()
        self._runtime_capability: RuntimeCapability | None = None

    @property
    def name(self) -> str:
        return "cuda_fixture"

    @property
    def runtime_capability(self) -> RuntimeCapability | None:
        return self._runtime_capability

    def load(self, model_spec: dict[str, Any]) -> None:
        self._runtime_capability = None
        try:
            config = read_artifact_config(model_spec)
        except ArtifactConfigError as exc:
            raise BackendError(ERR_CONFIG_INVALID, str(exc)) from exc
        if config.get("device") != "cuda":
            raise BackendError(
                ERR_CONFIG_INVALID,
                "CUDA fixture config requires device='cuda'",
            )
        try:
            import torch
        except Exception as exc:
            raise BackendError(ERR_LOAD_FAILED, "PyTorch is required for the CUDA fixture") from exc

        self._runtime_capability = require_cuda_runtime(torch)
        try:
            probe = torch.ones((_PROBE_EXTENT, _PROBE_EXTENT), device="cuda")
            observed = float((probe @ probe).sum().item())
            torch.cuda.synchronize()
        except Exception as exc:
            raise BackendError(
                ERR_LOAD_FAILED,
                "CUDA fixture tensor operation failed",
                runtime_capability=self._runtime_capability,
            ) from exc
        # An unchecked kernel would also pass on a card that computed
        # nothing, which is the failure this gate exists to catch.
        expected = float(_PROBE_EXTENT**3)
        if observed != expected:
            raise BackendError(
                ERR_LOAD_FAILED,
                f"CUDA fixture matmul returned {observed}, expected {expected}",
                runtime_capability=self._runtime_capability,
            )
        super().load(model_spec)

    def unload(self) -> None:
        super().unload()
        self._runtime_capability = None


__all__ = ["CudaFixtureBackend"]

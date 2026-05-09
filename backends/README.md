# `backends/`

Out-of-process backend implementations that the C++ runtime drives via the
adapter interface plus a versioned IPC boundary.

C++ in-process backend adapters (TensorRT first, optional in-process
LibTorch in a later milestone) live under `runtime/src/` and stay subject
to the public `ExecutionSession` contract. This directory is for backends
that are deliberately *not* in the C++ hot path — typically because their
SDK is Python-native or because we want process isolation between the
data plane and the model framework.

## Members

| Path | Language | Purpose |
| --- | --- | --- |
| [`python_pytorch/`](python_pytorch/) | Python | PyTorch fallback / reference backend used for SmolVLA validation per the v0.1.0 roadmap. Lower-risk path while the TensorRT performance path lands. |

## Ownership and rules

- **Layer:** data plane (out-of-process backend)
- **Owner:** runtime tech lead (interface) + per-backend owner (implementation)
- Backends here speak the runtime protocol over a versioned IPC boundary.
  They must not link against `runtime/`, `serving_worker/`, `agent/`,
  `cli/`, or `observability/`.
- Backends are gated by capability flags published through the adapter
  interface (V01-E05); a backend that lacks a feature reports it with a
  typed `Unsupported` error rather than silently degrading.
- Vendor SDK packaging (PyTorch, CUDA wheels, etc.) is system-provided.
  We do not vendor framework binaries.

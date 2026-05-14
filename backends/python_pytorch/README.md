# `backends/python_pytorch/`

`tensorplate-pytorch-backend` — the Python-language PyTorch backend
service used as TensorPlate's fallback / reference path and as the
required v0.1.0 SmolVLA validation route when TensorRT is not feasible.

## Ownership

- **Layer:** data plane (out-of-process backend)
- **Language:** Python (3.10+)
- **Package:** `tensorplate-pytorch-backend` (PEP 621 `pyproject.toml`)
- **Process model:** runs as its own service supervised by
  `tensorplate-agent`; communicates with `tensorplate-serving` through
  the runtime's versioned IPC boundary (no in-process FFI in v0.1.0).

## Scope (v0.1.0)

V01-E01-F01 only ships the package skeleton, build metadata, and quality
gates so contributors and CI have a stable home for the implementation
that lands in V01-E05 (backend adapter baseline). Specifically, this
milestone provides:

- A `pyproject.toml` declaring the package, Python ≥ 3.10, license, and
  ruff / mypy / pytest configuration.
- A namespace package at
  `src/tensorplate_pytorch_backend/__init__.py` with version constants
  that mirror the runtime's `kRuntimeVersion` / `kProtocolVersion`.
- A smoke test under `tests/` so `pytest` proves the package is
  importable and consistent with the C++ and Rust version surfaces.
- A `python.yml` CI workflow running ruff, mypy, and pytest.

V01-E05 lands the IPC runner, dependency-free fixture backend, and C++
sidecar adapter used to prove the process/socket boundary. Real
TorchScript / `torch.compile` / SmolVLA model loading stays out of the
host-CI baseline and lands with the model-specific validation work.
Published capabilities must match the C++ adapter: native async,
generation, streaming, KV-cache, and fixed-shape support remain disabled
unless a concrete backend implements them.

## Why not in-process LibTorch?

LibTorch is supported in the C++ adapter registry (see the
`libtorch-adapter` feature flag in `vcpkg.json`), but the Python backend
is the v0.1.0 default for three reasons:

1. **SmolVLA tooling.** SmolVLA / LeRobot ship with PyTorch-first
   tooling; staying in Python avoids re-deriving conversion paths.
2. **Process isolation.** Putting PyTorch in its own process keeps
   framework bugs and OOMs out of the serving worker.
3. **Schedule risk.** A LibTorch in-process path is feasible but pulls
   the LibTorch SDK into the C++ build matrix; deferring that to V01-E05
   keeps V01-E01 scope small.

## Local development

```bash
# Install the package in editable mode with dev dependencies (PEP 660).
pip install -e ".[dev]"

# Quality gates that mirror .github/workflows/python.yml:
ruff check .
ruff format --check .
mypy src tests
pytest -q
```

PyTorch itself is **not** pinned in `pyproject.toml` for V01-E01-F01;
it joins the dependency set in V01-E05 along with the chosen wheel
matrix (CPU, CUDA, Jetson aarch64).

## Rules

- `tensorplate-pytorch-backend` does **not** import from any C++ runtime
  module. Communication is over the IPC contract defined by
  `protocol/schemas/`.
- Hardware-boundary errors surface as typed exceptions translated to
  `Result<T>::Error` on the wire (V01-E02 contract).
- Capabilities (async / generation / streaming / KV-cache / fixed-shape)
  are published through the adapter interface and must never be lied
  about.

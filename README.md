<p align="center">
  <img src="docs/assets/readme_banner.png" alt="TensorPlate" width="100%">
</p>

# TensorPlate

[![C++](https://github.com/tensorplate/tensorplate/actions/workflows/cpp.yml/badge.svg)](https://github.com/tensorplate/tensorplate/actions/workflows/cpp.yml)
[![Rust](https://github.com/tensorplate/tensorplate/actions/workflows/rust.yml/badge.svg)](https://github.com/tensorplate/tensorplate/actions/workflows/rust.yml)
[![Python](https://github.com/tensorplate/tensorplate/actions/workflows/python.yml/badge.svg)](https://github.com/tensorplate/tensorplate/actions/workflows/python.yml)
[![Release](https://github.com/tensorplate/tensorplate/actions/workflows/release.yml/badge.svg)](https://github.com/tensorplate/tensorplate/actions/workflows/release.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](LICENSE)

TensorPlate is an inference platform for edge AI and robotics — reliable, observable model serving on hardware-constrained devices, with a C++ runtime hot path and Rust control plane.

> **Status:** v0.1.0 release-candidate. Tooling, packaging, validation reports, and install docs are in place; the public release must still be cut from a clean `release/v0.1.0` branch and annotated `v0.1.0` tag.

## Features

- Hardware-adjacent inference runtime for Jetson- and Kria-class edge devices.
- Serving worker with explicit lifecycle, readiness, and health contracts.
- Rust device agent for deployment, rollback, supervision, and desired-state reconciliation.
- Adapter-based backends (TensorRT, PyTorch / LibTorch).
- Tiered tests: unit, integration, adapter contract, hardware-in-loop, and benchmark.

## Getting Started

**Install from a release.** Start with [external-install.md](docs/install/external-install.md), then [quickstart.md](docs/install/quickstart.md). Release process and tag policy: [docs/release/](docs/release/).

**Build from source.** Read [CONTRIBUTING.md](CONTRIBUTING.md), then follow [local-validation.md](docs/contributing/local-validation.md) for the exact CMake, ctest, Cargo, and lint commands CI runs. For Jetson cross-compilation, see [jetson-cross-compile.md](docs/contributing/jetson-cross-compile.md).

## Repository Layout

```text
include/tensorplate/         Public C++ headers
runtime/                     Core inference runtime (C++20)
serving_worker/              Data-plane worker process (C++20)
backends/python_pytorch/     Out-of-process Python/PyTorch backend
agent/                       Rust device agent
cli/                         Rust operator CLI
observability/               Rust independent health monitor
protocol/                    Cross-component schemas + Rust crate
config/schemas/              Deployment and runtime config schemas
test/                        Unit, integration, contract, HIL, benchmark
cmake/                       Toolchains, modules, feature flags
docs/                        Architecture and contributing docs
```

Package owners, allowed dependencies, and review gates: [docs/architecture/ownership.md](docs/architecture/ownership.md).

## Contributing

Issues are organized as **Epic → Feature → Task**, plus **Bug** for regressions and contract violations. Branches are named after issues (`git checkout -b issue-##`). Pull requests should link an issue, cover acceptance criteria, include test evidence, and note changelog impact. Release branches use `release/vX.Y.Z`; final tags are annotated `vX.Y.Z` and are never retagged ([policy](docs/release/version-tag-policy.md)).

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full contribution contract.

## Security

Do not report security issues through public GitHub issues. See [SECURITY.md](SECURITY.md).

## License

Apache License 2.0. See [LICENSE](LICENSE).

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

### Install From A Release

On a supported target (Jetson Orin Nano 8GB Super, JetPack 6.x, `arm64`):

```bash
export TP_VERSION=0.1.0
export TP_TAG="v${TP_VERSION}"
export TP_DEBIAN_VERSION="${TP_VERSION}-1"
export TP_ARCH=arm64
export TP_RELEASE_URL="https://github.com/tensorplate/tensorplate/releases/download/${TP_TAG}"

mkdir -p "/tmp/tensorplate-${TP_TAG}" && cd "/tmp/tensorplate-${TP_TAG}"
for pkg in common agent serving observability cli; do
  curl -fL -O "${TP_RELEASE_URL}/tensorplate-${pkg}_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb"
done
curl -fL -O "${TP_RELEASE_URL}/SHA256SUMS" && sha256sum -c SHA256SUMS

sudo apt install ./tensorplate-*_${TP_DEBIAN_VERSION}_${TP_ARCH}.deb
sudo systemctl enable --now tensorplate-agent tensorplate-observability
tensorplate doctor
```

Full guide, troubleshooting, and the optional Python/PyTorch backend: [external-install.md](docs/install/external-install.md). Then walk through [quickstart.md](docs/install/quickstart.md).

### Build From Source

```bash
# C++ runtime and serving worker
cmake -S . -B build -G Ninja \
  -DCMAKE_BUILD_TYPE=RelWithDebInfo \
  -DCMAKE_TOOLCHAIN_FILE="$VCPKG_ROOT/scripts/buildsystems/vcpkg.cmake" \
  -DVCPKG_CHAINLOAD_TOOLCHAIN_FILE="$PWD/cmake/toolchains/x86_64-linux-gnu.cmake"
cmake --build build --parallel
ctest --test-dir build --output-on-failure -L T1

# Rust agent, CLI, observability
cargo test --workspace --no-fail-fast
```

Prerequisites, lint/format checks, and the full CI-equivalent sequence: [local-validation.md](docs/contributing/local-validation.md). For Jetson cross-compilation, see [jetson-cross-compile.md](docs/contributing/jetson-cross-compile.md). Release process and tag policy: [docs/release/](docs/release/).

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

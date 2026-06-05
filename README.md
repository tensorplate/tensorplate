<p align="center">
  <img src="docs/assets/tenosrplate_banner.png" alt="TensorPlate" width="100%">
</p>


<p align="center">
  <a href="https://github.com/tensorplate/tensorplate/actions/workflows/cpp.yml"><img src="https://github.com/tensorplate/tensorplate/actions/workflows/cpp.yml/badge.svg" alt="C++"></a>
  <a href="https://github.com/tensorplate/tensorplate/actions/workflows/rust.yml"><img src="https://github.com/tensorplate/tensorplate/actions/workflows/rust.yml/badge.svg" alt="Rust"></a>
  <a href="https://github.com/tensorplate/tensorplate/actions/workflows/python.yml"><img src="https://github.com/tensorplate/tensorplate/actions/workflows/python.yml/badge.svg" alt="Python"></a>
  <a href="https://github.com/tensorplate/tensorplate/actions/workflows/release.yml"><img src="https://github.com/tensorplate/tensorplate/actions/workflows/release.yml/badge.svg" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache_2.0-blue.svg" alt="License: Apache 2.0"></a>
</p>




> **Status:** v0.1.0 release-candidate. Tooling, packaging, validation reports, and install docs are in place; the public release must still be cut from a clean `release/v0.1.0` branch and annotated `v0.1.0` tag.

# TensorPlate


TensorPlate provides a full stack inference runtime for physical AI with observable model serving on hardware-constrained devices. It's built with a C++ runtime hot path and Rust control plane.


## Features

Ship AI models to NVIDIA Jetson and AMD Kria devices and keep them serving in
the field, without writing your own deployment, supervision, and health
tooling. 

TensorPlate is the runtime and control plane that production physical AI inference needs:

- **Serve models with low, predictable latency.** A C++ inference runtime runs
  your model on-device through TensorRT, LibTorch, or PyTorch. You can swap backends
  per deployment without changing your client.
- **Deploy and roll back safely.** Push a new model with one CLI command; if it
  fails to come up, the agent rolls back automatically. 
- **Survive crashes unattended.** The device agent supervises the serving
  worker, restarts it on failure, and recovers cleanly across reboots so a
  remote box stays serving without a human on site.
- **Know when it's actually healthy.** Explicit readiness and health endpoints
  tell callers when it's safe to send traffic, and an independent monitor keeps
  reporting health and metrics even if the main process is degraded.
- **Operate it from one CLI.** `deploy`, `rollback`, `status`, `infer`, `logs`,
  and `doctor`. It's everything you need to run a fleet, scriptable for CI.

## Getting Started

### Install From A Release

On a target [supported hardware](https://tensorplate.com/docs/hardware/overview) (e.g. Jetson Orin Nano 8GB Super, JetPack 6.x, `arm64`):

```bash
curl -fLO https://github.com/tensorplate/tensorplate/releases/download/v0.1.0/install.sh && sudo bash install.sh
```

For a desktop CLI-only install:

```bash
curl -fLO https://github.com/tensorplate/tensorplate/releases/download/v0.1.0/install.sh && sudo bash install.sh --cli-only
```

Before a release exists, build and install an unreleased branch snapshot:

```bash
curl -fL https://raw.githubusercontent.com/tensorplate/tensorplate/develop/packaging/scripts/build-install-from-source.sh -o build-install-from-source.sh && sudo bash build-install-from-source.sh --branch develop
```

Full guide, troubleshooting, and source-install caveats: [Installation](https://tensorplate.com/docs/installation)

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

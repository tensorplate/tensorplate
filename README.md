<p align="center">
  <img src="docs/assets/tensorplate_banner.png" alt="TensorPlate" width="100%">
</p>

<h3 align="center">
The Inference Layer for Physical AI
</h3>

<p align="center">
  <a href="https://github.com/tensorplate/tensorplate/actions/workflows/cpp.yml"><img src="https://github.com/tensorplate/tensorplate/actions/workflows/cpp.yml/badge.svg" alt="C++"></a>
  <a href="https://github.com/tensorplate/tensorplate/actions/workflows/rust.yml"><img src="https://github.com/tensorplate/tensorplate/actions/workflows/rust.yml/badge.svg" alt="Rust"></a>
  <a href="https://github.com/tensorplate/tensorplate/actions/workflows/python.yml"><img src="https://github.com/tensorplate/tensorplate/actions/workflows/python.yml/badge.svg" alt="Python"></a>
  <a href="https://github.com/tensorplate/tensorplate/actions/workflows/release.yml"><img src="https://github.com/tensorplate/tensorplate/actions/workflows/release.yml/badge.svg" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-Apache_2.0-blue.svg" alt="License: Apache 2.0"></a>
</p>

<p align="center">
<a href="https://tensorplate.com"><b>Site</b></a> | <a href="https://tensorplate.com/docs"><b>Documentation</b></a> | <a href="https://x.com/tensorplatehq"><b>X (Twitter)</b></a>
</p>

# TensorPlate


Production-grade model serving for physical AI. TensorPlate runs
models on Jetson-class edge devices and handles deployment, rollback, and
health so they keep serving unattended.


## Features

Ship AI models to edge hardware and keep them serving in the field,
without writing your own deployment, supervision, and health tooling.

TensorPlate is the runtime and control plane that production physical AI inference needs:

- **Serve models with low, predictable latency.** A C++ inference runtime runs
  your model on-device. The v0.1.1 packaged release supports TensorRT on
  Jetson and an optional out-of-process Python/PyTorch backend; future
  backends are reserved by the bundle and adapter interfaces.
- **Deploy and roll back safely.** Push a new model with one CLI command; if it
  fails to come up, the agent rolls back automatically.
- **Survive crashes unattended.** The device agent supervises the serving
  worker, restarts it on failure, and recovers cleanly across reboots so a
  remote box stays serving without a human on site.
- **Know when it's actually healthy.** Explicit readiness and health endpoints
  tell callers when it's safe to send traffic, and an independent monitor keeps
  reporting health and metrics even if the main process is degraded.
- **Operate it from one CLI.** `deploy`, `rollback`, `status`, `infer`, `logs`,
  and `doctor` cover the local device workflow and are scriptable for CI.

## Getting Started

### Install

On a TensorPlate-ready Jetson (the TensorPlate APT source ships
preconfigured on [supported hardware](https://tensorplate.com/docs/hardware/overview),
e.g. Jetson Orin Nano 8GB Super, JetPack 6.x, `arm64`), the entire
runtime install is:

```bash
sudo apt update
sudo apt install tensorplate
```

On a stock Ubuntu/Jetson host, configure the TensorPlate APT source once,
then use the same two commands — APT cannot discover TensorPlate until
this one-time bootstrap runs:

```bash
curl -fLO https://github.com/tensorplate/tensorplate/releases/download/v0.1.2/tensorplate-apt-source_0.1.2-1_all.deb
sudo dpkg -i tensorplate-apt-source_0.1.2-1_all.deb
sudo apt update && sudo apt install tensorplate
```

The source points at the stable channel, so future releases arrive
through normal `apt update` / `apt upgrade`; the bootstrap never repeats.
See [tensorplate-ready.md](docs/install/tensorplate-ready.md) for
provisioning, validation, and upgrade flows.

CLI-only workstations:

```bash
sudo apt install tensorplate-cli            # Ubuntu AMD64, after the same one-time bootstrap
brew install tensorplate/tap/tensorplate    # macOS Apple Silicon (CLI-only by design)
```

The signed GitHub Release assets and `install.sh` remain the supported
no-APT fallback ([external-install.md](docs/install/external-install.md)).
Before a release exists, build and install an unreleased branch snapshot:

```bash
curl -fL https://raw.githubusercontent.com/tensorplate/tensorplate/develop/packaging/scripts/build-install-from-source.sh -o build-install-from-source.sh && sudo bash build-install-from-source.sh --branch develop
```

Full guide, troubleshooting, and source-install caveats: [Installation](https://tensorplate.com/docs/installation)

### Call Deployed Models From Python

The first-party `tensorplate-python` SDK calls models you have already
deployed, over the serving `/infer` envelope:

```python
import tensorplate

client = tensorplate.VisionClient("http://127.0.0.1:18080")
detections = client.detect("frame.jpg", endpoint="yolov8n")
```

Install (`pip install tensorplate-python`), quickstart, and the
API reference: [docs/sdk/](docs/sdk/).

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

**Data plane** — the C++20 hot path that runs the model.

| Component | Path | What it is |
|---|---|---|
| Inference runtime | [runtime/](runtime/) | Core on-device execution engine (sessions, scheduling, buffers) |
| Serving worker | [serving_worker/](serving_worker/) | Data-plane worker process that serves inference requests |
| Python backend | [backends/python_pytorch/](backends/python_pytorch/) | Out-of-process Python/PyTorch backend |
| Public headers | [include/tensorplate/](include/tensorplate/) | Public C++ API |

**Control plane** — the Rust services that deploy, supervise, and observe.

| Component | Path | What it is |
|---|---|---|
| Device agent | [agent/](agent/) | Deployment, rollback, supervision, desired-state reconciliation |
| Operator CLI | [cli/](cli/) | Command-line interface for running a fleet |
| Health monitor | [observability/](observability/) | Independent health and metrics monitor |

**Shared contracts & tooling**

| Component | Path | What it is |
|---|---|---|
| Protocol | [protocol/](protocol/) | Cross-component JSON Schemas + Rust crate |
| Config schemas | [config/schemas/](config/schemas/) | Deployment and runtime config schemas |
| Tests | [test/](test/) | Unit, integration, contract, HIL, benchmark |
| Build | [cmake/](cmake/) | Toolchains, modules, feature flags |
| Docs | [docs/](docs/) | Architecture and contributing docs |
| Python SDK | [sdk/python/](sdk/python/) | First-party client SDK for calling deployed models from Python |

Package owners, allowed dependencies, and review gates: [docs/architecture/ownership.md](docs/architecture/ownership.md).

## Contributing

Issues are organized as **Epic → Feature → Task**, plus **Bug** for regressions and contract violations. Branches are named after issues (`git checkout -b issue-##`). Pull requests should link an issue, cover acceptance criteria, include test evidence, and note changelog impact. Release branches use the per-minor `release/X.Y` maintenance line; final tags are annotated `vX.Y.Z` and are never retagged ([policy](docs/release/version-tag-policy.md)).

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full contribution contract.

## Security

Do not report security issues through public GitHub issues. See [SECURITY.md](SECURITY.md).

## License

Apache License 2.0. See [LICENSE](LICENSE).

# TensorPlate

TensorPlate is an inference platform for edge AI and robotics. It is designed for reliable, observable model serving on hardware-constrained devices, with a C++ runtime hot path and Rust control-plane components.

> Status: early OSS setup. The repository is being scaffolded before the first public implementation release.

## What TensorPlate Is

TensorPlate is intended to provide:

- A hardware-adjacent inference runtime for devices such as Jetson and Kria-class edge systems.
- A serving worker with explicit lifecycle, readiness, and health contracts.
- A Rust device agent for deployment, rollback, supervision, and desired-state reconciliation.
- Adapter-based backend support for runtimes such as TensorRT and ONNX Runtime.
- A test strategy that separates unit, integration, adapter contract, hardware-in-loop, and benchmark validation.

## Architecture Principles

TensorPlate contributions should preserve these core constraints:

- Runtime hot-path and serving-worker code is C++20.
- Device agent, watchdog, and CLI code is Rust.
- Python SDK code is a thin HTTP API wrapper.
- Runtime behavior that varies by deployment belongs in config.
- Fallible hardware-boundary operations return `Result<T>`.
- Cross-layer payloads use value objects and `BufferRef` / `TensorView`.
- Hardware resources are owned through RAII wrappers inside adapter internals.
- Dependencies flow downward through the runtime layers.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the working contribution contract.

## Planned Repository Layout

```text
include/tensorplate/    Public C++ headers
runtime/                Core inference runtime
serving_worker/         Data-plane worker process
agent/                  Rust device agent
watchdog/               Rust safety monitor
cli/                    Rust operator CLI
sdk/python/             Python SDK
test/                   Unit, integration, contract, HIL, and benchmark tests
config/schemas/         Deployment and runtime config schemas
protocol/               Language-neutral schemas and generated bindings
cmake/                  Toolchains, modules, and feature flags
docs/                   Public architecture and contributing docs
```

## Getting Started

There is no supported build yet. For now:

1. Read [CONTRIBUTING.md](CONTRIBUTING.md).
2. Use the GitHub issue templates for Epics, Features, Tasks, and Bugs.
3. Keep public behavior, interface, config schema, feature flag, and error-code changes reflected in [CHANGELOG.md](CHANGELOG.md).

Build and development instructions will be added once the initial CMake, Cargo, and devcontainer scaffolding lands.

## Contributing

Issues are organized as:

- Epic: a roadmap outcome.
- Feature: a deliverable slice under an Epic.
- Task: a concrete implementation, test, documentation, or validation item under a Feature.
- Bug: incorrect behavior, regression, crash, or contract violation.

Pull requests should link an issue, describe acceptance criteria coverage, include test evidence, and note changelog impact.

## Security

Please do not report security issues through public GitHub issues. See [SECURITY.md](SECURITY.md).

## License

TensorPlate is licensed under the Apache License 2.0. See [LICENSE](LICENSE).

# Changelog

All notable changes to TensorPlate will be documented in this file.

This project follows the spirit of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and uses semantic versioning once public releases begin.

## [Unreleased]

### Added

- Top-level package skeleton for v0.1.0: `include/tensorplate/`, `runtime/`,
  `serving_worker/`, `agent/`, `cli/`, `observability/`, `protocol/schemas/`,
  `protocol/rust/`, `config/schemas/`, `test/`, `cmake/`, and
  `docs/architecture/` (V01-E01-F01).
- `docs/architecture/ownership.md` documenting per-package owners, allowed
  dependencies, and forbidden upward dependencies (V01-E01-F01-T02).
- Test tree layout for tiers T1 through T5 plus shared mocks and model
  fixtures, documented in `test/README.md` (V01-E01-F01-T03).
- Root CMake build with `tp_runtime` (alias `tp::runtime`) static library,
  `tp_serving_worker` binary (output `tensorplate-serving`), and CTest
  wiring with T1 label (V01-E01-F02-T01).
- vcpkg manifest (`vcpkg.json`) declaring the GoogleTest dependency and
  reserving feature flags for adapter SDKs; toolchain stubs
  `cmake/toolchains/x86_64-linux-gnu.cmake` and
  `cmake/toolchains/aarch64-jetson.cmake` (V01-E01-F02-T02).
- `cmake/features/warnings.cmake` and `cmake/features/sanitizers.cmake`
  helpers; `TP_ENABLE_SANITIZERS` and `TP_WARNINGS_AS_ERRORS` options;
  `tp_test_unit` GoogleTest target with smoke coverage (V01-E01-F02-T03).
- `.clang-format` and `.clang-tidy` baseline configurations.
- Cargo workspace at the repository root with members `tensorplate-agent`,
  `tensorplate-cli`, `tensorplate-observability`, and `tensorplate-protocol`,
  pinned `rust-toolchain.toml` (1.78.0), `rustfmt.toml` baseline, and
  workspace-wide rustc and clippy lints (V01-E01-F03-T01).
- Crate entrypoints with version banners and a baseline test in
  `tensorplate-protocol` proving workspace builds end to end without
  device hardware (V01-E01-F03-T02).
- Documented Rust quality commands (`cargo build`, `cargo test`,
  `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`) in `CONTRIBUTING.md` (V01-E01-F03-T03).
- `.github/workflows/cpp.yml` running the C++ build, T1 unit tests in a
  release and ASAN/UBSAN matrix, `clang-format --dry-run -Werror`, and
  `clang-tidy` against the exported compile commands (V01-E01-F04-T01).
- `.github/workflows/rust.yml` running `cargo fmt --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, and
  `cargo test --workspace` against the pinned toolchain (V01-E01-F04-T02).
- vcpkg and Cargo dependency caching, per-workflow concurrency, and a
  documented PR / nightly / release-branch status policy in
  `CONTRIBUTING.md` (V01-E01-F04-T03).
- `.devcontainer/Dockerfile` and `.devcontainer/devcontainer.json`
  delivering a reproducible Ubuntu 22.04 dev image with CMake, Ninja,
  Clang 15, clang-format/-tidy, vcpkg, and the pinned Rust toolchain;
  named volumes mount the vcpkg and cargo caches across rebuilds
  (V01-E01-F05-T01).
- `docs/contributing/jetson-cross-compile.md` documenting the supported
  cross-compile path: `cmake/toolchains/aarch64-jetson.cmake`,
  TP_JETSON_SYSROOT/CC/CXX inputs, and JetPack/TensorRT/CUDA system
  ownership (V01-E01-F05-T02).
- `docs/contributing/local-validation.md` enumerating the canonical
  CMake/CTest/clang-format/clang-tidy and Cargo commands that mirror CI
  (V01-E01-F05-T03).
- `include/tensorplate/version.hpp` (generated from `.hpp.in` by CMake's
  `configure_file`) exposing four independent version surfaces:
  `kRuntimeVersion`, `kProtocolVersion`, `kBundleFormatVersion`, and the
  per-component MAJOR/MINOR/PATCH constants (V01-E01-F06-T01).
- `tensorplate_protocol::PROTOCOL_VERSION_*` and
  `BUNDLE_FORMAT_VERSION_*` Rust constants mirroring the C++ surface
  (V01-E01-F06-T01).
- T1 unit tests (C++ `version_test.cpp`, Rust `tests` module in
  `protocol/rust`) verifying that composed version strings agree with
  their components on each side (V01-E01-F06-T01).
- `docs/architecture/versioning.md` documenting the runtime / protocol /
  schema / bundle-format surfaces, bump rules, and the planned
  compatibility-validation path (V01-E01-F06-T02).
- `CONTRIBUTING.md` "Release and Changelog Policy" section listing the
  changes that require a `CHANGELOG.md` entry plus a version bump
  (V01-E01-F06-T03).
- `backends/python_pytorch/` package skeleton for the out-of-process
  PyTorch backend per the V01-E01 scope expansion: PEP 621
  `pyproject.toml`, namespace package
  `tensorplate_pytorch_backend` with mirrored protocol/bundle-format
  version constants, `py.typed` marker, ruff/ruff-format/mypy/pytest
  configuration, and a smoke test suite (V01-E01-F01).
- `.github/workflows/python.yml` running `ruff check`,
  `ruff format --check`, `mypy src tests`, and `pytest -q` against
  Python 3.10 and 3.12 on Ubuntu 22.04 with pip caching.
- `docs/architecture/ownership.md` updated with the new package row,
  out-of-process IPC dependency arrow, and forbidden-dependency rule
  preventing the Python backend from linking against any C++ runtime
  module.
- `include/tensorplate/core/error.hpp` defining the `tensorplate::Error`
  value object and the stable `Error::Code` taxonomy
  (`ConfigInvalid`, `LoadFailed`, `NotReady`, `ShapeMismatch`,
  `Unsupported`, `OOMError`, `Timeout`, `InferenceFailed`, `Internal`)
  with snake_case `to_string` / `error_code_from_string` helpers
  (V01-E02-F01-T01).
- `include/tensorplate/core/result.hpp` providing
  `tensorplate::Result<T>` (and `Result<void>`) with std::expected-shaped
  semantics and a `tp` namespace alias for the planning-doc API surface
  (V01-E02-F01-T01).
- `protocol/schemas/error.json` (JSON Schema Draft 7) and Rust mirror
  `tensorplate_protocol::ProtocolError` / `ErrorCode`, plus
  `decode_with_version_check` and `DecodeError` enforcing typed
  rejection of unknown `schema_version` values (V01-E02-F01-T02).
- T1 unit tests for `Error`, `Result<T>`, the protocol round-trip, and
  unknown-schema-version rejection (V01-E02-F01-T03).

### Changed

- `README.md` repository layout block now reflects the realized v0.1.0
  package skeleton and links to the ownership document.

### Deprecated

### Removed

### Fixed

### Security

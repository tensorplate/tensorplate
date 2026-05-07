# Changelog

All notable changes to TensorPlate will be documented in this file.

This project follows the spirit of [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and uses semantic versioning once public releases begin.

## [Unreleased]

### Added

- Top-level package skeleton for v0.1: `include/tensorplate/`, `runtime/`,
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

### Changed

- `README.md` repository layout block now reflects the realized v0.1
  package skeleton and links to the ownership document.

### Deprecated

### Removed

### Fixed

### Security
